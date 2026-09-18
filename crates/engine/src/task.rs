//! Process-wide task runtime.
//! Core only forwards [`TaskHost`] to tools. This module owns the durable state transitions and
//! the process-local [`RuntimeHandle`] registry, so a turn ending never drops background work.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use async_trait::async_trait;
use chrono::{Datelike, Timelike, Utc};
use chrono_tz::Tz;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use zlogic_core::SharedStore;
use zlogic_objects::ObjectStore;
use zlogic_protocol::query::{
    ApiError, ApiResult, RuntimeTask, RuntimeTaskDeleteReq, RuntimeTaskKind, RuntimeTaskListReq,
    RuntimeTaskLog, RuntimeTaskLogReq, RuntimeTaskPage, RuntimeTaskState, RuntimeTaskStopReq,
    TaskJob, TaskJobConcurrencyPolicy, TaskJobCreateReq, TaskJobDeleteReq, TaskJobExecutor,
    TaskJobListReq, TaskJobRunReq, TaskJobRunsReq, TaskJobSchedule, TaskJobSetEnabledReq,
};
use zlogic_protocol::stream::{OutputStream, TaskOutputDelta};
use zlogic_protocol::{MessagePart, SessionId, TaskUpdatePart};
use zlogic_store::{Delivery, NewSession, TitleSource};
use zlogic_task::{
    AgentResult, AgentSpec, ConcurrencyPolicy, ExecutorSpec, JobDefinition, JobStore, NewJob,
    NewTask, PermissionPolicy, ProcessResult, ProcessSpec, RuntimeHandle, Schedule, TaskId,
    TaskResult, TaskRun, TaskState, TaskStore, TaskTrigger,
};
use zlogic_tools::{
    AgentMailboxGate, AgentRequest, AgentSpawner, ProcessRequest, SpawnedProcess, TaskHost,
    TaskReport,
};

use crate::service::TaskService;

#[derive(Clone)]
pub struct TaskManager {
    inner: Arc<Inner>,
}

struct Inner {
    store: SharedStore,
    objects: Arc<dyn ObjectStore>,
    spool_dir: PathBuf,
    runtimes: Mutex<HashMap<TaskId, RuntimeHandle>>,
    agent_mailboxes: Mutex<HashMap<TaskId, Arc<AgentMailboxGate>>>,
    waker: Mutex<Option<Weak<dyn TaskWake>>>,
    agent_factory: Mutex<Option<Weak<dyn ScheduledAgentFactory>>>,
    hub: Mutex<Option<Weak<crate::hub::EventHub>>>,
    scheduler_started: AtomicBool,
}

/// The narrow edge from a finished runtime back into conversation dispatch.
/// TaskManager keeps only a weak reference: Dispatcher owns CoreServices, which owns TaskManager.
/// A strong edge here would make the whole engine a reference cycle.
pub(crate) trait TaskWake: Send + Sync {
    fn mailbox_ready(&self, session_id: SessionId) -> Result<(), String>;
}

struct TaskConsole {
    hub: Arc<crate::hub::EventHub>,
}

impl TaskConsole {
    fn publish(&self, task_id: TaskId, stream: OutputStream, chunk: &[u8]) {
        let text = String::from_utf8_lossy(chunk);
        if text.is_empty() {
            return;
        }
        self.hub.emit_task(TaskOutputDelta {
            task_id: task_id.to_string(),
            stream,
            chunk: text.into_owned(),
        });
    }
}

/// Why a background run stopped early.
/// Which cancellation token fired is the whole difference between "the user stopped this task"
/// and "the turn that created it was stopped", and only the first one may reach the conversation.
/// A completion notification writes a mailbox row and wakes the session, which **opens a new
/// turn**: sent for a task the creating turn's own stop just killed, it would hand the model a
/// `task_update` and the reply the user pressed stop on would carry straight on from there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// The run's own token — the task panel's stop button. The user asked about *this* run, so its
    /// terminal state is news the conversation wants.
    Requested,
    /// The creating turn's token — the user pressed stop in the composer. The turn is already
    /// ending around this, and nobody is waiting for the result.
    TurnEnded,
}

impl Stop {
    /// Whether the conversation should be told. False only for a stop the conversation itself
    /// caused.
    const fn notifies(self) -> bool {
        matches!(self, Self::Requested)
    }
}

#[async_trait]
pub(crate) trait ScheduledAgentFactory: Send + Sync {
    async fn spawner_for(
        &self,
        workspace_id: zlogic_protocol::WorkspaceId,
        parent_session_id: SessionId,
        profile: &str,
        model_ref: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<Arc<dyn AgentSpawner>, String>;
}

impl TaskManager {
    pub fn new(
        store: SharedStore,
        objects: Arc<dyn ObjectStore>,
        spool_dir: impl Into<PathBuf>,
    ) -> Result<Self, String> {
        store
            .with(|db| zlogic_task::install_schema(db.conn()))
            .map_err(|error| error.to_string())?;
        let spool_dir = spool_dir.into();
        std::fs::create_dir_all(&spool_dir).map_err(|error| {
            format!(
                "cannot create task output directory {}: {error}",
                spool_dir.display()
            )
        })?;
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                objects,
                spool_dir,
                runtimes: Mutex::new(HashMap::new()),
                agent_mailboxes: Mutex::new(HashMap::new()),
                waker: Mutex::new(None),
                agent_factory: Mutex::new(None),
                hub: Mutex::new(None),
                scheduler_started: AtomicBool::new(false),
            }),
        })
    }

    pub(crate) fn bind_hub(&self, hub: Weak<crate::hub::EventHub>) {
        *self
            .inner
            .hub
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hub);
    }

    pub(crate) fn bind_waker(&self, waker: Weak<dyn TaskWake>) {
        *self
            .inner
            .waker
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(waker);
    }

    pub(crate) fn bind_agent_factory(&self, factory: Weak<dyn ScheduledAgentFactory>) {
        *self
            .inner
            .agent_factory
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(factory);
    }

    pub fn store(&self) -> &SharedStore {
        &self.inner.store
    }

    /// Close executions whose futures belonged to the previous process.
    /// This runs before the scheduler. Using the normal finish path makes the state change and its
    /// Task Session mailbox notification one durable transaction.
    pub fn reconcile_interrupted(&self) -> Result<usize, String> {
        let running = self
            .inner
            .store
            .with(|db| TaskStore::new(db.conn()).list_recoverable())
            .map_err(|error| error.to_string())?;
        let mut reconciled = 0;
        for task in running {
            if self.inner.finish_checked(
                task.task_id,
                task.state,
                TaskState::Interrupted,
                task.result,
                Some("runtime restarted before the task completed"),
                true,
            ) {
                reconciled += 1;
            }
        }
        Ok(reconciled)
    }

    fn register_process(
        &self,
        task: TaskRun,
        mut process: SpawnedProcess,
        launch_cancel: zlogic_tools::CancellationToken,
    ) -> TaskId {
        let task_id = task.task_id;
        let inner = self.inner.clone();
        let (registered_tx, registered_rx) = oneshot::channel();
        let handle = RuntimeHandle::spawn(task_id, move |cancel| async move {
            let _ = registered_rx.await;
            // The turn may already be over by the time this runtime is first polled: the tool
            // handed the process to the background and the user pressed stop in the same instant.
            // Both tokens mean "do not run"; only the run's own token means anyone wants to hear
            // that it never started.
            let stopped_before_start = if cancel.is_cancelled() {
                Some(Stop::Requested)
            } else if launch_cancel.is_cancelled() {
                Some(Stop::TurnEnded)
            } else {
                None
            };
            if let Some(stop) = stopped_before_start {
                terminate_process(&mut process.child).await;
                inner.finish_stopped(task_id, TaskState::Queued, stop, None);
                inner.runtimes().remove(&task_id);
                return;
            }
            if let Err(error) =
                inner.transition(task_id, TaskState::Queued, TaskState::Running, None, None)
            {
                terminate_process(&mut process.child).await;
                tracing::error!(target: "zlogic::task", %task_id, "could not claim task: {error}");
                inner.runtimes().remove(&task_id);
                return;
            }

            let spool_path = inner.spool_path(task_id);
            let mut spool = match tokio::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&spool_path)
                .await
            {
                Ok(file) => file,
                Err(error) => {
                    terminate_process(&mut process.child).await;
                    inner.finish(
                        task_id,
                        TaskState::Running,
                        TaskState::Failed,
                        None,
                        Some(&format!("cannot persist process output: {error}")),
                    );
                    inner.runtimes().remove(&task_id);
                    return;
                }
            };

            // Nothing has been read from this process before adoption: the single entry point into
            // the background is `shell`'s `background: true`, which decides before the first byte.
            let mut output_chars = 0u64;
            let console = inner.console();

            let mut stdout_buf = [0u8; 8192];
            let mut stderr_buf = [0u8; 8192];
            let mut stdout_open = true;
            let mut stderr_open = true;
            let mut cancelled = false;
            let mut write_error = None;
            while stdout_open || stderr_open {
                tokio::select! {
                    read = process.stdout.read(&mut stdout_buf), if stdout_open => match read {
                        Ok(0) | Err(_) => stdout_open = false,
                        Ok(read) => {
                            let chunk = &stdout_buf[..read];
                            output_chars += String::from_utf8_lossy(chunk).chars().count() as u64;
                            if let Err(error) = spool.write_all(chunk).await {
                                write_error = Some(error.to_string());
                                break;
                            }
                            if let Some(console) = &console {
                                console.publish(task_id, OutputStream::Stdout, chunk);
                            }
                        }
                    },
                    read = process.stderr.read(&mut stderr_buf), if stderr_open => match read {
                        Ok(0) | Err(_) => stderr_open = false,
                        Ok(read) => {
                            let chunk = &stderr_buf[..read];
                            output_chars += String::from_utf8_lossy(chunk).chars().count() as u64;
                            if let Err(error) = spool.write_all(chunk).await {
                                write_error = Some(error.to_string());
                                break;
                            }
                            if let Some(console) = &console {
                                console.publish(task_id, OutputStream::Stderr, chunk);
                            }
                        }
                    },
                    () = cancel.cancelled() => {
                        cancelled = true;
                        break;
                    },
                    () = launch_cancel.cancelled() => {
                        cancelled = true;
                        break;
                    },
                }
            }

            // Which token fired decides whether the conversation hears about this. Read here
            // rather than inside the arms above so a near-simultaneous pair of stops is decided
            // the same way every time: the stop aimed at *this* task is the one that counts, and
            // it is the one whose notification the user is waiting for.
            let stop = if !cancelled {
                None
            } else if cancel.is_cancelled() {
                Some(Stop::Requested)
            } else {
                Some(Stop::TurnEnded)
            };

            if cancelled || write_error.is_some() {
                terminate_process(&mut process.child).await;
            }
            let status = process.child.wait().await;
            let _ = spool.flush().await;
            drop(spool);

            let object_id = if output_chars == 0 {
                None
            } else {
                let objects = inner.objects.clone();
                let path = spool_path.clone();
                match tokio::task::spawn_blocking(move || objects.put_path(&path)).await {
                    Ok(Ok(id)) => Some(id),
                    Ok(Err(error)) => {
                        write_error = Some(format!("cannot store process output: {error}"));
                        None
                    }
                    Err(error) => {
                        write_error = Some(format!("output storage worker failed: {error}"));
                        None
                    }
                }
            };

            let exit_code = status
                .as_ref()
                .ok()
                .and_then(std::process::ExitStatus::code);
            let result = Some(TaskResult::Process(ProcessResult {
                exit_code,
                output_object_id: object_id,
                output_chars,
            }));
            if let Some(stop) = stop {
                inner.finish_stopped(task_id, TaskState::Running, stop, result);
            } else if let Some(error) = write_error {
                inner.finish(
                    task_id,
                    TaskState::Running,
                    TaskState::Failed,
                    result,
                    Some(&error),
                );
            } else {
                match status {
                    Ok(status) if status.success() => inner.finish(
                        task_id,
                        TaskState::Running,
                        TaskState::Succeeded,
                        result,
                        None,
                    ),
                    Ok(status) => {
                        let reason = match status.code() {
                            Some(code) => format!("process exited with code {code}"),
                            None => "process was killed by a signal".into(),
                        };
                        inner.finish(
                            task_id,
                            TaskState::Running,
                            TaskState::Failed,
                            result,
                            Some(&reason),
                        );
                    }
                    Err(error) => inner.finish(
                        task_id,
                        TaskState::Running,
                        TaskState::Failed,
                        result,
                        Some(&format!("could not wait for process: {error}")),
                    ),
                }
            }
            inner.runtimes().remove(&task_id);
        });

        self.inner.runtimes().insert(task_id, handle);
        let _ = registered_tx.send(());
        task_id
    }

    fn register_agent(
        &self,
        task: TaskRun,
        mut request: AgentRequest,
        spawner: Arc<dyn AgentSpawner>,
    ) -> TaskId {
        let task_id = task.task_id;
        let inner = self.inner.clone();
        let mailbox = Arc::new(AgentMailboxGate::default());
        request.mailbox = Some(mailbox.clone());
        inner
            .agent_mailboxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(task_id, mailbox.clone());
        let runtime_mailbox = mailbox;
        let launch_cancel = request.cancel.clone();
        let (registered_tx, registered_rx) = oneshot::channel();
        let handle = RuntimeHandle::spawn(task_id, move |cancel| async move {
            let _ = registered_rx.await;
            // Same window as a process run: the turn may already be over by the first poll. The
            // run's own token means someone is waiting to hear it never started; the turn's own
            // token means the turn is going away and its conversation must stay out of it.
            let stopped_before_start = if cancel.is_cancelled() {
                Some(Stop::Requested)
            } else if launch_cancel.is_cancelled() {
                Some(Stop::TurnEnded)
            } else {
                None
            };
            if let Some(stop) = stopped_before_start {
                inner.finish_stopped(task_id, TaskState::Queued, stop, None);
                inner.runtimes().remove(&task_id);
                inner
                    .agent_mailboxes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&task_id);
                runtime_mailbox.close().await;
                return;
            }
            if let Err(error) =
                inner.transition(task_id, TaskState::Queued, TaskState::Running, None, None)
            {
                tracing::error!(target: "zlogic::task", %task_id, "could not claim task: {error}");
                inner.runtimes().remove(&task_id);
                inner
                    .agent_mailboxes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&task_id);
                runtime_mailbox.close().await;
                return;
            }

            let run_cancel = zlogic_tools::CancellationToken::new();
            let bridged = run_cancel.clone();
            let task_cancel = cancel.clone();
            // Which token fired has to survive the child run: a stop the user aimed at this task
            // is news for the conversation, the creating turn's own stop is not. Written before
            // the cancel that lets the await below return, so the read after it cannot miss it.
            let stopped_by_turn = Arc::new(AtomicBool::new(false));
            let turn_flag = stopped_by_turn.clone();
            let cancellation_bridge = tokio::spawn(async move {
                // `biased` with the task's own token first: when both stops land together, the one
                // aimed at this task is the one that counts, exactly as in the process loop.
                let by_turn = tokio::select! {
                    biased;
                    () = task_cancel.cancelled() => false,
                    () = launch_cancel.cancelled() => true,
                };
                turn_flag.store(by_turn, Ordering::SeqCst);
                bridged.cancel();
            });
            request.cancel = run_cancel.clone();
            request.unattended = true;
            let outcome = spawner.spawn(request).await;
            // CoreSpawner closes the gate on both success and failure; this close only marks gates
            // that never activated (failures before a child session was created). After close the
            // gate keeps the session id, so the failure branches below can still link the run to
            // its persisted transcript.
            runtime_mailbox.close().await;
            cancellation_bridge.abort();
            let child_session_id = runtime_mailbox.session_id().await;
            if run_cancel.is_cancelled() {
                let stop = if stopped_by_turn.load(Ordering::SeqCst) {
                    Stop::TurnEnded
                } else {
                    Stop::Requested
                };
                inner.finish_stopped(
                    task_id,
                    TaskState::Running,
                    stop,
                    child_session_id.map(|child_session_id| {
                        TaskResult::Agent(AgentResult {
                            child_session_id,
                            final_entry_id: None,
                            conclusion: None,
                        })
                    }),
                );
            } else {
                match outcome {
                    Ok(outcome) => inner.finish(
                        task_id,
                        TaskState::Running,
                        TaskState::Succeeded,
                        Some(TaskResult::Agent(AgentResult {
                            child_session_id: outcome.session_id,
                            final_entry_id: None,
                            conclusion: Some(outcome.answer),
                        })),
                        None,
                    ),
                    Err(error) => inner.finish(
                        task_id,
                        TaskState::Running,
                        TaskState::Failed,
                        child_session_id.map(|child_session_id| {
                            TaskResult::Agent(AgentResult {
                                child_session_id,
                                final_entry_id: None,
                                conclusion: None,
                            })
                        }),
                        Some(&error),
                    ),
                }
            }
            inner.runtimes().remove(&task_id);
            inner
                .agent_mailboxes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&task_id);
        });
        self.inner.runtimes().insert(task_id, handle);
        let _ = registered_tx.send(());
        task_id
    }

    /// Starts the lightweight scheduler the first time a host reaches the task API. Bootstrap may
    /// happen before a Tokio runtime exists, so starting it in `new` is unsafe.
    pub fn start_scheduler(&self) -> bool {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        if self.inner.scheduler_started.swap(true, Ordering::AcqRel) {
            return true;
        }
        let manager = self.clone();
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Err(error) = manager.scheduler_tick().await {
                    tracing::warn!(target: "zlogic::task", "scheduler tick failed: {error}");
                }
            }
        });
        true
    }

    async fn scheduler_tick(&self) -> Result<(), String> {
        let jobs = self
            .inner
            .store
            .with(|db| JobStore::new(db.conn()).list_enabled())
            .map_err(|error| error.to_string())?;
        let now = Utc::now();
        for job in jobs {
            let scheduled_for = match due_at(&job.schedule, now) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(target: "zlogic::task", job_id = %job.job_id, "{error}");
                    continue;
                }
            };
            let Some(scheduled_for) = scheduled_for else {
                continue;
            };
            let already_created = self
                .inner
                .store
                .with(|db| {
                    Ok::<_, zlogic_task::StoreError>(
                        TaskStore::new(db.conn())
                            .list_for_job(job.job_id)?
                            .into_iter()
                            .any(|task| task.scheduled_for == Some(scheduled_for)),
                    )
                })
                .map_err(|error| error.to_string())?;
            if already_created {
                continue;
            }
            if let Err(error) = self
                .launch_job(
                    job.clone(),
                    TaskTrigger::Scheduled { scheduled_for },
                    Some(scheduled_for),
                )
                .await
            {
                tracing::warn!(target: "zlogic::task", "scheduled job could not start: {error}");
            } else if matches!(job.schedule, Schedule::Once { .. })
                && let Err(error) = self
                    .inner
                    .store
                    .with(|db| JobStore::new(db.conn()).set_enabled(job.job_id, false))
            {
                tracing::warn!(
                    target: "zlogic::task",
                    job_id = %job.job_id,
                    "could not disable completed one-shot job: {error}"
                );
            }
        }
        Ok(())
    }

    async fn launch_job(
        &self,
        job: JobDefinition,
        trigger: TaskTrigger,
        scheduled_for: Option<chrono::DateTime<Utc>>,
    ) -> Result<TaskRun, String> {
        let active = self
            .inner
            .store
            .with(|db| TaskStore::new(db.conn()).list_active_for_job(job.job_id))
            .map_err(|error| error.to_string())?;
        match job.concurrency_policy {
            ConcurrencyPolicy::Forbid if !active.is_empty() => {
                return Err(format!("job {} already has an active run", job.job_id));
            }
            ConcurrencyPolicy::Replace => {
                for task in active {
                    let runtimes = self.inner.runtimes();
                    if let Some(handle) = runtimes.get(&task.task_id) {
                        handle.cancel();
                    } else {
                        return Err(format!(
                            "job {} has an active run owned by another process",
                            job.job_id
                        ));
                    }
                }
            }
            _ => {}
        }

        let task = self
            .inner
            .store
            .with(|db| {
                TaskStore::new(db.conn()).create(NewTask {
                    job_id: Some(job.job_id),
                    workspace_id: job.workspace_id,
                    executor: job.executor.clone(),
                    permission_policy: job.permission_policy,
                    notification_session_id: job.notification_session_id,
                    trigger,
                    turn_scoped: false,
                    attempt: 1,
                    scheduled_for,
                })
            })
            .map_err(|error| error.to_string())?;

        let spec = match &job.executor {
            ExecutorSpec::Process(spec) => spec.clone(),
            ExecutorSpec::Agent(spec) => {
                let prepared = async {
                    let factory = self
                        .inner
                        .agent_factory
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_ref()
                        .and_then(Weak::upgrade)
                        .ok_or_else(|| "scheduled agent runtime is not available".to_string())?;
                    let parent_session_id = job.notification_session_id.ok_or_else(|| {
                        format!("agent job {} has no notification session", job.job_id)
                    })?;
                    let spawner = factory
                        .spawner_for(
                            job.workspace_id,
                            parent_session_id,
                            &spec.agent,
                            spec.model_ref.as_deref(),
                            spec.cwd.as_deref(),
                        )
                        .await?;
                    Ok::<_, String>((parent_session_id, spawner))
                }
                .await;
                let (parent_session_id, spawner) = match prepared {
                    Ok(value) => value,
                    Err(reason) => {
                        self.inner.finish(
                            task.task_id,
                            TaskState::Queued,
                            TaskState::Failed,
                            None,
                            Some(&reason),
                        );
                        return Err(reason);
                    }
                };
                let task_id = self.register_agent(
                    task,
                    AgentRequest {
                        agent: spec.agent.clone(),
                        task: spec.prompt.clone(),
                        parent_session_id,
                        parent_turn_id: zlogic_protocol::TurnId::new(),
                        exec_cwd: spec.cwd.clone(),
                        mailbox: None,
                        anchor_call_id: zlogic_protocol::CallId::new(format!("job:{}", job.job_id)),
                        unattended: true,
                        cancel: zlogic_tools::CancellationToken::new(),
                        system: None,
                        tools: None,
                        model: None,
                    },
                    spawner,
                );
                return self
                    .inner
                    .task(task_id)?
                    .ok_or_else(|| format!("task {task_id} disappeared after launch"));
            }
        };

        let root = self
            .inner
            .store
            .with(|db| db.workspaces().get(job.workspace_id))
            .map_err(|error| error.to_string())?
            .path;
        let cwd = spec.cwd.clone().unwrap_or(root);
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&cwd)
            .envs(&spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        // The backgrounded process is a console binary (the resolved shell, or a server the shell
        // launched). From a GUI host it must not open a console window: run it headless.
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        #[cfg(unix)]
        command.process_group(0);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let reason = format!("cannot start {}: {error}", spec.program);
                self.inner.finish(
                    task.task_id,
                    TaskState::Queued,
                    TaskState::Failed,
                    None,
                    Some(&reason),
                );
                return Err(reason);
            }
        };
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let task_id = self.register_process(
            task,
            SpawnedProcess {
                child,
                stdout,
                stderr,
            },
            zlogic_tools::CancellationToken::new(),
        );
        self.inner
            .task(task_id)?
            .ok_or_else(|| format!("task {task_id} disappeared after launch"))
    }

    #[cfg(test)]
    fn runtime_count(&self) -> usize {
        self.inner.runtimes().len()
    }

    async fn runtime_tasks_with_live_sessions(&self, tasks: Vec<TaskRun>) -> Vec<RuntimeTask> {
        let gates: Vec<(TaskRun, Option<Arc<AgentMailboxGate>>)> = {
            let mailboxes = self
                .inner
                .agent_mailboxes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            tasks
                .into_iter()
                .map(|task| {
                    let gate = match &task.executor {
                        ExecutorSpec::Agent(_) => mailboxes.get(&task.task_id).cloned(),
                        _ => None,
                    };
                    (task, gate)
                })
                .collect()
        };
        let mut out = Vec::with_capacity(gates.len());
        for (task, gate) in gates {
            let running_session = match gate {
                Some(gate) => gate.session_id().await.map(|id| id.to_string()),
                None => None,
            };
            out.push(runtime_task(task, running_session.as_deref()));
        }
        out
    }
}

#[async_trait]
impl TaskService for TaskManager {
    async fn list_tasks(&self, req: RuntimeTaskListReq) -> ApiResult<RuntimeTaskPage> {
        self.start_scheduler();
        let limit = req.stopped_limit.unwrap_or(20).clamp(1, 100);
        let (active, stopped, stopped_total) = self
            .inner
            .store
            .with(|db| {
                let tasks = TaskStore::new(db.conn());
                Ok::<_, zlogic_task::StoreError>((
                    tasks.list_active_for_workspace(req.workspace_id, req.only_job_tasks)?,
                    tasks.list_terminal_for_workspace(
                        req.workspace_id,
                        req.stopped_offset,
                        limit,
                        req.only_job_tasks,
                    )?,
                    tasks.count_terminal_for_workspace(req.workspace_id, req.only_job_tasks)?,
                ))
            })
            .map_err(|error| ApiError::internal(format!("failed to read tasks: {error}")))?;
        let active = self.runtime_tasks_with_live_sessions(active).await;
        Ok(RuntimeTaskPage {
            active,
            stopped: stopped.into_iter().map(|t| runtime_task(t, None)).collect(),
            stopped_total,
        })
    }

    async fn job_runs(&self, req: TaskJobRunsReq) -> ApiResult<RuntimeTaskPage> {
        self.start_scheduler();
        let job_id = req.job_id.parse().map_err(|error| {
            ApiError::invalid_code("job_id_invalid", format!("invalid job id: {error}"))
        })?;
        let limit = req.stopped_limit.unwrap_or(20).clamp(1, 100);
        let (active, stopped, stopped_total) = self
            .inner
            .store
            .with(|db| {
                let tasks = TaskStore::new(db.conn());
                JobStore::new(db.conn()).get(job_id)?;
                Ok::<_, zlogic_task::StoreError>((
                    tasks.list_active_for_job(job_id)?,
                    tasks.list_terminal_for_job(job_id, req.stopped_offset, limit)?,
                    tasks.count_terminal_for_job(job_id)?,
                ))
            })
            .map_err(|error| {
                ApiError::invalid_code(
                    "task_job_not_found",
                    format!("failed to read job runs: {error}"),
                )
            })?;
        let active = self.runtime_tasks_with_live_sessions(active).await;
        Ok(RuntimeTaskPage {
            active,
            stopped: stopped.into_iter().map(|t| runtime_task(t, None)).collect(),
            stopped_total,
        })
    }

    async fn stop_task(&self, req: RuntimeTaskStopReq) -> ApiResult<()> {
        let task_id = req.task_id.parse().map_err(|error| {
            ApiError::invalid_code("task_id_invalid", format!("invalid task id: {error}"))
        })?;
        let task = self
            .inner
            .store
            .with(|db| TaskStore::new(db.conn()).get(task_id))
            .map_err(|error| ApiError::invalid_code("task_not_found", error.to_string()))?;
        if task.workspace_id != req.workspace_id {
            return Err(ApiError::invalid_code(
                "task_workspace_mismatch",
                "Task does not belong to this workspace",
            ));
        }
        let task_session_id = task
            .notification_session_id
            .ok_or_else(|| ApiError::internal(format!("Task {task_id} has no task session")))?;
        <Self as TaskHost>::stop(self, task_session_id, task_id)
            .await
            .map_err(|error| ApiError::conflict_code("task_stop_conflict", error))
    }

    async fn delete_task(&self, req: RuntimeTaskDeleteReq) -> ApiResult<()> {
        let task_id = req.task_id.parse().map_err(|error| {
            ApiError::invalid_code("task_id_invalid", format!("invalid task id: {error}"))
        })?;
        let task = self
            .inner
            .store
            .with(|db| TaskStore::new(db.conn()).find(task_id))
            .map_err(|error| ApiError::internal(format!("failed to read task: {error}")))?
            .ok_or_else(|| ApiError::invalid_code("task_not_found", "task not found"))?;
        if task.workspace_id != req.workspace_id {
            return Err(ApiError::invalid_code(
                "task_workspace_mismatch",
                "Task does not belong to this workspace",
            ));
        }
        if matches!(
            task.state,
            zlogic_task::TaskState::Queued | zlogic_task::TaskState::Running
        ) {
            return Err(ApiError::conflict_code(
                "task_stop_conflict",
                "task is still running; stop it first",
            ));
        }
        let removed = self
            .inner
            .store
            .with(|db| {
                let tx = db.conn().unchecked_transaction()?;
                let tasks = TaskStore::new(&tx);
                let removed = tasks.delete(task_id)?;
                if removed && req.remove_orphan_job {
                    if let Some(job_id) = task.job_id {
                        let remaining = TaskStore::new(&tx).list_for_job(job_id)?;
                        if remaining.is_empty() {
                            JobStore::new(&tx).delete(job_id).ok();
                        }
                    }
                }
                tx.commit()?;
                Ok::<_, zlogic_task::StoreError>(removed)
            })
            .map_err(|error| ApiError::internal(format!("failed to delete task: {error}")))?;
        if removed {
            let path = self.inner.spool_path(task_id);
            let _ = std::fs::remove_file(&path);
        }
        Ok(())
    }

    async fn task_log(&self, req: RuntimeTaskLogReq) -> ApiResult<RuntimeTaskLog> {
        let task_id = req.task_id.parse().map_err(|error| {
            ApiError::invalid_code("task_id_invalid", format!("invalid task id: {error}"))
        })?;
        let task = self
            .inner
            .store
            .with(|db| TaskStore::new(db.conn()).get(task_id))
            .map_err(|error| ApiError::invalid_code("task_not_found", error.to_string()))?;
        if task.workspace_id != req.workspace_id {
            return Err(ApiError::invalid_code(
                "task_workspace_mismatch",
                "Task does not belong to this workspace",
            ));
        }
        let state = match task.state {
            TaskState::Queued => RuntimeTaskState::Queued,
            TaskState::Running => RuntimeTaskState::Running,
            TaskState::NeedsInput => RuntimeTaskState::NeedsInput,
            TaskState::Succeeded => RuntimeTaskState::Succeeded,
            TaskState::Failed => RuntimeTaskState::Failed,
            TaskState::Cancelled => RuntimeTaskState::Cancelled,
            TaskState::Interrupted => RuntimeTaskState::Interrupted,
        };
        let kind = match &task.executor {
            ExecutorSpec::Process(_) => RuntimeTaskKind::Process,
            ExecutorSpec::Agent(_) => RuntimeTaskKind::Agent,
        };
        let error = task.error.clone();

        match &task.executor {
            ExecutorSpec::Process(_) => {
                let path = self.inner.spool_path(task_id);
                let (text, truncated) = tokio::task::spawn_blocking(move || {
                    let text = read_spool_preview(&path)?;
                    let truncated = std::fs::metadata(&path)
                        .map(|meta| meta.len() as usize > PROCESS_HEAD_BYTES + PROCESS_TAIL_BYTES)
                        .unwrap_or(false);
                    Ok::<_, String>((text, truncated))
                })
                .await
                .map_err(|error| ApiError::internal(format!("failed to read task log: {error}")))?
                .map_err(ApiError::internal)?;
                let (result, output_object_id) = match &task.result {
                    Some(TaskResult::Process(result)) => (
                        task.state.is_terminal().then(|| text.clone()),
                        result.output_object_id.as_ref().map(|id| id.to_string()),
                    ),
                    _ => (None, None),
                };
                Ok(RuntimeTaskLog {
                    task_id: task.task_id.to_string(),
                    state,
                    kind,
                    text,
                    truncated,
                    result,
                    child_session_id: None,
                    output_object_id,
                    error,
                })
            }
            ExecutorSpec::Agent(_) => {
                let child_session_id = match &task.result {
                    Some(TaskResult::Agent(result)) => Some(result.child_session_id),
                    _ => {
                        let gate = self
                            .inner
                            .agent_mailboxes
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .get(&task_id)
                            .cloned();
                        match gate {
                            Some(gate) => gate.session_id().await,
                            None => None,
                        }
                    }
                };
                let result = match &task.result {
                    Some(TaskResult::Agent(result)) => result.conclusion.clone(),
                    _ => None,
                };
                Ok(RuntimeTaskLog {
                    task_id: task.task_id.to_string(),
                    state,
                    kind,
                    text: String::new(),
                    truncated: false,
                    result,
                    child_session_id: child_session_id.map(|id| id.to_string()),
                    output_object_id: None,
                    error,
                })
            }
        }
    }

    async fn list_jobs(&self, req: TaskJobListReq) -> ApiResult<Vec<TaskJob>> {
        self.start_scheduler();
        self.inner
            .store
            .with(|db| JobStore::new(db.conn()).list(req.workspace_id))
            .map_err(|error| ApiError::internal(format!("failed to read jobs: {error}")))?
            .into_iter()
            .map(task_job)
            .collect()
    }

    async fn create_job(&self, req: TaskJobCreateReq) -> ApiResult<TaskJob> {
        self.start_scheduler();
        let title = req.title.trim();
        if title.is_empty() {
            return Err(ApiError::invalid_code(
                "task_job_invalid",
                "Job name cannot be empty",
            ));
        }
        let schedule = schedule_from_api(req.schedule)?;
        let workspace = self
            .inner
            .store
            .with(|db| db.workspaces().get(req.workspace_id))
            .map_err(|error| {
                ApiError::invalid_code(
                    "task_job_scope_invalid",
                    format!("invalid workspace: {error}"),
                )
            })?;
        let executor = match req.executor {
            TaskJobExecutor::Process { program, args, cwd } => {
                let program = program.trim();
                if program.is_empty() {
                    return Err(ApiError::invalid_code(
                        "task_job_invalid",
                        "executable program cannot be empty",
                    ));
                }
                ExecutorSpec::Process(ProcessSpec {
                    program: program.to_string(),
                    args,
                    cwd: validate_job_cwd(&workspace.path, cwd.as_deref())?,
                    env: Default::default(),
                })
            }
            TaskJobExecutor::Agent {
                profile,
                prompt,
                cwd,
                model_ref,
            } => {
                let profile = profile.trim();
                let prompt = prompt.trim();
                if profile.is_empty() || prompt.is_empty() {
                    return Err(ApiError::invalid_code(
                        "task_job_invalid",
                        "agent profile and task description cannot be empty",
                    ));
                }
                ExecutorSpec::Agent(AgentSpec {
                    prompt: prompt.to_string(),
                    agent: profile.to_string(),
                    model_ref: model_ref
                        .map(|m| m.trim().to_string())
                        .filter(|m| !m.is_empty()),
                    cwd: validate_job_cwd(&workspace.path, cwd.as_deref())?,
                })
            }
        };
        let job = self
            .inner
            .store
            .with(|db| {
                let tx = db.conn().unchecked_transaction()?;
                let sessions = zlogic_store::SessionStore::new(&tx);
                let task_session = match req.task_session_id {
                    Some(session_id) => {
                        let session = sessions.get(session_id)?;
                        if session.workspace_id != req.workspace_id
                            || !session.is_root()
                            || !session.is_task_session()
                        {
                            return Err(zlogic_store::StoreError::Corrupt(
                                "task session does not belong to this workspace or has the \
                                 wrong type"
                                    .into(),
                            ));
                        }
                        session
                    }
                    None => sessions.create(NewSession::task(req.workspace_id))?,
                };
                sessions.set_title(task_session.session_id, title, TitleSource::User)?;
                let job = JobStore::new(&tx)
                    .create(NewJob {
                        workspace_id: req.workspace_id,
                        owner: zlogic_task::JobOwner::User,
                        title: title.to_string(),
                        executor,
                        schedule,
                        enabled: req.enabled,
                        concurrency_policy: concurrency_from_api(req.concurrency_policy),
                        permission_policy: PermissionPolicy::DenyRequests,
                        notification_session_id: Some(task_session.session_id),
                    })
                    .map_err(|error| zlogic_store::StoreError::Corrupt(error.to_string()))?;
                tx.commit()?;
                Ok(job)
            })
            .map_err(|error| ApiError::internal(format!("failed to create job: {error}")))?;
        task_job(job)
    }

    async fn set_job_enabled(&self, req: TaskJobSetEnabledReq) -> ApiResult<TaskJob> {
        self.start_scheduler();
        let job_id = req.job_id.parse().map_err(|error| {
            ApiError::invalid_code("job_id_invalid", format!("invalid job id: {error}"))
        })?;
        let job = self
            .inner
            .store
            .with(|db| JobStore::new(db.conn()).set_enabled(job_id, req.enabled))
            .map_err(|error| {
                ApiError::invalid_code(
                    "task_job_update_failed",
                    format!("failed to update job: {error}"),
                )
            })?;
        task_job(job)
    }

    async fn run_job(&self, req: TaskJobRunReq) -> ApiResult<RuntimeTask> {
        self.start_scheduler();
        let job_id = req.job_id.parse().map_err(|error| {
            ApiError::invalid_code("job_id_invalid", format!("invalid job id: {error}"))
        })?;
        let job = self
            .inner
            .store
            .with(|db| JobStore::new(db.conn()).get(job_id))
            .map_err(|error| {
                ApiError::invalid_code("task_job_not_found", format!("failed to read job: {error}"))
            })?;
        self.launch_job(job, TaskTrigger::Manual, None)
            .await
            .map(|task| runtime_task(task, None))
            .map_err(|error| ApiError::conflict_code("task_job_run_conflict", error))
    }

    async fn delete_job(&self, req: TaskJobDeleteReq) -> ApiResult<()> {
        self.start_scheduler();
        let job_id = req.job_id.parse().map_err(|error| {
            ApiError::invalid_code("job_id_invalid", format!("invalid job id: {error}"))
        })?;
        self.inner
            .store
            .with(|db| JobStore::new(db.conn()).delete(job_id))
            .map_err(|error| {
                ApiError::invalid_code(
                    "task_job_delete_failed",
                    format!("failed to delete job: {error}"),
                )
            })
    }
}

fn task_job(job: JobDefinition) -> ApiResult<TaskJob> {
    let executor = match job.executor {
        ExecutorSpec::Process(spec) => TaskJobExecutor::Process {
            program: spec.program,
            args: spec.args,
            cwd: spec.cwd,
        },
        ExecutorSpec::Agent(spec) => TaskJobExecutor::Agent {
            profile: spec.agent,
            prompt: spec.prompt,
            cwd: spec.cwd,
            model_ref: spec.model_ref,
        },
    };
    let notification_session_id = job.notification_session_id.ok_or_else(|| {
        ApiError::internal(format!("Job {} has no notification session", job.job_id))
    })?;
    Ok(TaskJob {
        job_id: job.job_id.to_string(),
        workspace_id: job.workspace_id,
        title: job.title,
        executor,
        schedule: schedule_to_api(job.schedule),
        enabled: job.enabled,
        concurrency_policy: concurrency_to_api(job.concurrency_policy),
        task_session_id: notification_session_id,
        created_at: job.created_at,
        updated_at: job.updated_at,
    })
}

fn validate_job_cwd(workspace_root: &str, value: Option<&str>) -> ApiResult<Option<String>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    let candidate = if path.is_absolute() {
        path
    } else {
        PathBuf::from(workspace_root).join(path)
    };
    let canonical = candidate.canonicalize().map_err(|error| {
        ApiError::invalid_code(
            "task_job_cwd_invalid",
            format!("working directory unavailable: {error}"),
        )
    })?;
    let root = PathBuf::from(workspace_root)
        .canonicalize()
        .map_err(|error| {
            ApiError::invalid_code(
                "task_job_cwd_invalid",
                format!("workspace directory unavailable: {error}"),
            )
        })?;
    if !canonical.starts_with(&root) {
        return Err(ApiError::invalid_code(
            "task_job_cwd_invalid",
            "working directory must be inside the current workspace",
        ));
    }
    Ok(Some(zlogic_store::normalise(&canonical)))
}

pub fn schedule_from_api(value: TaskJobSchedule) -> ApiResult<Schedule> {
    match value {
        TaskJobSchedule::Manual => Ok(Schedule::Manual),
        TaskJobSchedule::Once { at } => {
            if at <= Utc::now() {
                return Err(ApiError::invalid_code(
                    "task_job_schedule_invalid",
                    "the scheduled run time must be later than now",
                ));
            }
            Ok(Schedule::Once { at })
        }
        TaskJobSchedule::Cron {
            expression,
            timezone,
        } => {
            let expression = expression.trim().to_string();
            CronSpec::parse(&expression)
                .map_err(|error| ApiError::invalid_code("task_job_schedule_invalid", error))?;
            timezone.parse::<Tz>().map_err(|_| {
                ApiError::invalid_code(
                    "task_job_schedule_invalid",
                    format!("unknown timezone: {timezone}"),
                )
            })?;
            Ok(Schedule::Cron {
                expression,
                timezone,
            })
        }
    }
}

fn schedule_to_api(value: Schedule) -> TaskJobSchedule {
    match value {
        Schedule::Manual => TaskJobSchedule::Manual,
        Schedule::Once { at } => TaskJobSchedule::Once { at },
        Schedule::Cron {
            expression,
            timezone,
        } => TaskJobSchedule::Cron {
            expression,
            timezone,
        },
    }
}

const fn concurrency_from_api(value: TaskJobConcurrencyPolicy) -> ConcurrencyPolicy {
    match value {
        TaskJobConcurrencyPolicy::Allow => ConcurrencyPolicy::Allow,
        TaskJobConcurrencyPolicy::Forbid => ConcurrencyPolicy::Forbid,
        TaskJobConcurrencyPolicy::Replace => ConcurrencyPolicy::Replace,
    }
}

const fn concurrency_to_api(value: ConcurrencyPolicy) -> TaskJobConcurrencyPolicy {
    match value {
        ConcurrencyPolicy::Allow => TaskJobConcurrencyPolicy::Allow,
        ConcurrencyPolicy::Forbid => TaskJobConcurrencyPolicy::Forbid,
        ConcurrencyPolicy::Replace => TaskJobConcurrencyPolicy::Replace,
    }
}

fn runtime_task(task: TaskRun, running_agent_session: Option<&str>) -> RuntimeTask {
    let (kind, title) = match &task.executor {
        ExecutorSpec::Process(spec) => {
            let mut title = spec.program.clone();
            if !spec.args.is_empty() {
                title.push(' ');
                title.push_str(&spec.args.join(" "));
            }
            (RuntimeTaskKind::Process, title)
        }
        ExecutorSpec::Agent(spec) => (
            RuntimeTaskKind::Agent,
            spec.prompt
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("Agent task")
                .trim()
                .to_string(),
        ),
    };
    let agent_session_id = match &task.result {
        Some(TaskResult::Agent(result)) => Some(result.child_session_id.to_string()),
        Some(TaskResult::Process(_)) => None,
        None => running_agent_session.map(str::to_string),
    };
    RuntimeTask {
        task_id: task.task_id.to_string(),
        state: match task.state {
            TaskState::Queued => RuntimeTaskState::Queued,
            TaskState::Running => RuntimeTaskState::Running,
            TaskState::NeedsInput => RuntimeTaskState::NeedsInput,
            TaskState::Succeeded => RuntimeTaskState::Succeeded,
            TaskState::Failed => RuntimeTaskState::Failed,
            TaskState::Cancelled => RuntimeTaskState::Cancelled,
            TaskState::Interrupted => RuntimeTaskState::Interrupted,
        },
        kind,
        title,
        job_id: task.job_id.map(|id| id.to_string()),
        detail: task.error.or_else(|| match task.result {
            Some(TaskResult::Agent(result)) => result.conclusion,
            Some(TaskResult::Process(result)) => Some(match result.exit_code {
                Some(code) => format!("exit code {code} · {} chars", result.output_chars),
                None => format!("{} chars", result.output_chars),
            }),
            None => None,
        }),
        agent_session_id,
        started_at: task.started_at,
        finished_at: task.finished_at,
        created_at: task.created_at,
    }
}

impl Inner {
    fn runtimes(&self) -> std::sync::MutexGuard<'_, HashMap<TaskId, RuntimeHandle>> {
        self.runtimes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn console(&self) -> Option<TaskConsole> {
        let hub = self
            .hub
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()?
            .upgrade()?;
        Some(TaskConsole { hub })
    }

    fn close_console(&self, task_id: TaskId) {
        if let Some(console) = self.console() {
            console.hub.close_task(&task_id.to_string());
        }
    }

    fn task(&self, task_id: TaskId) -> Result<Option<TaskRun>, String> {
        self.store
            .with(|db| TaskStore::new(db.conn()).find(task_id))
            .map_err(|error| error.to_string())
    }

    fn spool_path(&self, task_id: TaskId) -> PathBuf {
        self.spool_dir.join(format!("{task_id}.log"))
    }

    fn transition(
        &self,
        task_id: TaskId,
        from: TaskState,
        to: TaskState,
        result: Option<TaskResult>,
        error: Option<&str>,
    ) -> Result<TaskRun, String> {
        self.store
            .with(|db| TaskStore::new(db.conn()).transition(task_id, from, to, result, error))
            .map_err(|error| error.to_string())
    }

    fn finish(
        &self,
        task_id: TaskId,
        from: TaskState,
        to: TaskState,
        result: Option<TaskResult>,
        error: Option<&str>,
    ) {
        self.finish_checked(task_id, from, to, result, error, true);
    }

    /// A run that something stopped, in the one place that decides whether the conversation is
    /// told. `from` is the state it was in when the stop arrived; the reason text follows from it.
    fn finish_stopped(
        &self,
        task_id: TaskId,
        from: TaskState,
        stop: Stop,
        result: Option<TaskResult>,
    ) {
        let error = match from {
            TaskState::Queued => "stopped before the task started",
            _ => "stopped by request",
        };
        self.finish_checked(
            task_id,
            from,
            TaskState::Cancelled,
            result,
            Some(error),
            stop.notifies(),
        );
    }

    /// `notify` is false only for a [`Stop::TurnEnded`] run: see [`Stop`] for why the mailbox row
    /// and the wake-up must not happen there.
    fn finish_checked(
        &self,
        task_id: TaskId,
        from: TaskState,
        to: TaskState,
        result: Option<TaskResult>,
        error: Option<&str>,
        notify: bool,
    ) -> bool {
        let preview = match &result {
            Some(TaskResult::Process(_)) if notify => {
                let text = read_spool_window(
                    &self.spool_path(task_id),
                    NOTIFY_HEAD_BYTES,
                    NOTIFY_TAIL_BYTES,
                )
                .unwrap_or_else(|error| {
                    tracing::warn!(
                        target: "zlogic::task",
                        %task_id,
                        "cannot read spool for the completion notification preview: {error}"
                    );
                    String::new()
                });
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        };
        let finished = self.store.with(|db| {
            let tx = db
                .conn()
                .unchecked_transaction()
                .map_err(|error| error.to_string())?;
            let task = TaskStore::new(&tx)
                .transition(task_id, from, to, result, error)
                .map_err(|error| error.to_string())?;
            let mut notification_written = false;
            if notify
                && let Some(session_id) = task.notification_session_id
                && zlogic_store::SessionStore::new(&tx)
                    .find(session_id)
                    .map_err(|error| error.to_string())?
                    .is_some()
            {
                let summary = match (&task.result, &task.error) {
                    (Some(TaskResult::Agent(result)), None) => result.conclusion.clone(),
                    (Some(TaskResult::Process(result)), None) => Some(match result.exit_code {
                        Some(code) => format!("process exited with code {code}"),
                        None => "process ended without an exit code".into(),
                    }),
                    (_, Some(error)) => Some(error.clone()),
                    _ => None,
                };
                let (command, cwd, agent) = match &task.executor {
                    ExecutorSpec::Process(spec) => {
                        let mut line = spec.program.clone();
                        if !spec.args.is_empty() {
                            line.push(' ');
                            line.push_str(&spec.args.join(" "));
                        }
                        (Some(line), spec.cwd.clone(), None)
                    }
                    ExecutorSpec::Agent(spec) => (None, None, Some(spec.agent.clone())),
                };
                let source = match &task.trigger {
                    TaskTrigger::Manual => Some("manual"),
                    TaskTrigger::Tool { .. } => Some("tool"),
                    TaskTrigger::Scheduled { .. } => Some("scheduled"),
                    TaskTrigger::Retry { .. } => Some("retry"),
                }
                .map(str::to_string);
                let job_title = task
                    .job_id
                    .and_then(|job_id| JobStore::new(&tx).find(job_id).ok().flatten())
                    .map(|job| job.title);
                let payload = serde_json::to_value(vec![MessagePart::TaskUpdate {
                    update: TaskUpdatePart {
                        task_id: task.task_id.to_string(),
                        state: task.state.as_str().to_string(),
                        summary,
                        child_session_id: match &task.result {
                            Some(TaskResult::Agent(result)) => Some(result.child_session_id),
                            _ => None,
                        },
                        command,
                        cwd,
                        preview: preview.clone(),
                        agent,
                        source,
                        job_title,
                    },
                }])
                .map_err(|error| error.to_string())?;
                let client_request_id = format!("task:{}:{}", task.task_id, task.state.as_str());
                zlogic_store::MailboxStore::new(&tx)
                    .submit(session_id, &client_request_id, &payload, Delivery::Steer)
                    .map_err(|error| error.to_string())?;
                notification_written = true;
            }
            tx.commit().map_err(|error| error.to_string())?;
            Ok::<_, String>((task, notification_written))
        });

        let finished_ok = match finished {
            Ok((task, notification_written)) => {
                let waker = self
                    .waker
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .and_then(Weak::upgrade);
                if notification_written
                    && let (Some(waker), Some(session_id)) = (waker, task.notification_session_id)
                    && let Err(error) = waker.mailbox_ready(session_id)
                {
                    tracing::error!(
                        target: "zlogic::task",
                        %task_id,
                        "task notification is durable but its conversation could not be woken: {error}"
                    );
                }
                true
            }
            Err(error) => {
                tracing::error!(target: "zlogic::task", %task_id, "could not finish task: {error}");
                false
            }
        };

        self.close_console(task_id);
        finished_ok
    }
}

#[async_trait]
impl TaskHost for TaskManager {
    async fn start_process(
        &self,
        request: ProcessRequest,
        mut process: SpawnedProcess,
    ) -> Result<TaskId, String> {
        let parent = self
            .inner
            .store
            .with(|db| db.sessions().get(request.parent_session_id))
            .map_err(|error| error.to_string())?;
        let active = <Self as TaskHost>::list(self, request.parent_session_id)
            .await?
            .into_iter()
            .filter(|task| {
                matches!(task.executor, ExecutorSpec::Process(_)) && !task.state.is_terminal()
            })
            .count();
        if active >= 8 {
            terminate_process(&mut process.child).await;
            return Err(
                "this conversation already has 8 active process tasks; stop one first".into(),
            );
        }

        let task = match self.inner.store.with(|db| {
            TaskStore::new(db.conn()).create(NewTask {
                job_id: None,
                workspace_id: parent.workspace_id,
                executor: ExecutorSpec::Process(request.spec.clone()),
                permission_policy: PermissionPolicy::DenyRequests,
                notification_session_id: Some(request.parent_session_id),
                trigger: TaskTrigger::Tool {
                    session_id: request.parent_session_id,
                    turn_id: request.parent_turn_id,
                    call_id: request.anchor_call_id.clone(),
                },
                turn_scoped: request.turn_scoped,
                attempt: 1,
                scheduled_for: None,
            })
        }) {
            Ok(task) => task,
            Err(error) => {
                terminate_process(&mut process.child).await;
                return Err(error.to_string());
            }
        };

        Ok(self.register_process(task, process, request.cancel))
    }

    async fn start_agent(
        &self,
        request: AgentRequest,
        spawner: Arc<dyn AgentSpawner>,
    ) -> Result<TaskId, String> {
        let parent = self
            .inner
            .store
            .with(|db| db.sessions().get(request.parent_session_id))
            .map_err(|error| error.to_string())?;

        let task = self
            .inner
            .store
            .with(|db| {
                TaskStore::new(db.conn()).create(NewTask {
                    job_id: None,
                    workspace_id: parent.workspace_id,
                    executor: ExecutorSpec::Agent(AgentSpec {
                        prompt: request.task.clone(),
                        agent: request.agent.clone(),
                        model_ref: parent.model_ref.clone(),
                        cwd: parent.exec_cwd.clone(),
                    }),
                    permission_policy: PermissionPolicy::DenyRequests,
                    notification_session_id: Some(request.parent_session_id),
                    trigger: TaskTrigger::Tool {
                        session_id: request.parent_session_id,
                        turn_id: request.parent_turn_id,
                        call_id: request.anchor_call_id.clone(),
                    },
                    turn_scoped: false,
                    attempt: 1,
                    scheduled_for: None,
                })
            })
            .map_err(|error| error.to_string())?;

        Ok(self.register_agent(task, request, spawner))
    }

    async fn get(
        &self,
        session_id: zlogic_protocol::SessionId,
        task_id: TaskId,
    ) -> Result<Option<TaskRun>, String> {
        Ok(self
            .inner
            .task(task_id)?
            .filter(|task| task.notification_session_id == Some(session_id)))
    }

    async fn report(
        &self,
        session_id: zlogic_protocol::SessionId,
        task_id: TaskId,
    ) -> Result<Option<TaskReport>, String> {
        // Built on `get` so the visibility rule ("this conversation's task, or nothing") has one
        // implementation: a report must never see a task the caller could not stop.
        let Some(task) = <Self as TaskHost>::get(self, session_id, task_id).await? else {
            return Ok(None);
        };
        // The spool is written from the moment a process task starts, so a running one reports how
        // far it has got. An agent task has no spool — its output *is* the child session, and the
        // conclusion is already on the row.
        let output = match &task.executor {
            ExecutorSpec::Process(_) => {
                let window = read_spool_window(
                    &self.inner.spool_path(task_id),
                    PROCESS_HEAD_BYTES,
                    PROCESS_TAIL_BYTES,
                )?;
                (!window.is_empty()).then_some(window)
            }
            ExecutorSpec::Agent(_) => None,
        };
        Ok(Some(TaskReport { task, output }))
    }

    async fn list(&self, session_id: zlogic_protocol::SessionId) -> Result<Vec<TaskRun>, String> {
        self.inner
            .store
            .with(|db| TaskStore::new(db.conn()).list_for_session(session_id))
            .map_err(|error| error.to_string())
    }

    async fn send_agent_message(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        message: String,
    ) -> Result<(), String> {
        let Some(task) = self.get(session_id, task_id).await? else {
            return Err(format!("no task {task_id} in this conversation"));
        };
        if !matches!(task.executor, ExecutorSpec::Agent(_)) {
            return Err(format!("task {task_id} is not an agent"));
        }
        if task.state.is_terminal() {
            return Err(format!("task {task_id} already {}", task.state.as_str()));
        }
        let gate = self
            .inner
            .agent_mailboxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&task_id)
            .cloned()
            .ok_or_else(|| format!("agent task {task_id} is not running in this process"))?;
        let store = self.inner.store.clone();
        let request_id = format!(
            "task-message:{task_id}:{}",
            zlogic_protocol::SubmissionId::new()
        );
        gate.with_open_session(move |child_session_id| async move {
            let result = (|| {
                let payload = serde_json::to_value(vec![MessagePart::Text { text: message }])
                    .map_err(|error| error.to_string())?;
                store
                    .with(|db| {
                        db.mailbox().submit(
                            child_session_id,
                            &request_id,
                            &payload,
                            Delivery::Steer,
                        )
                    })
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })();
            (result, false)
        })
        .await
        .ok_or_else(|| format!("agent task {task_id} is no longer accepting messages"))?
    }

    async fn stop(
        &self,
        session_id: zlogic_protocol::SessionId,
        task_id: TaskId,
    ) -> Result<(), String> {
        let Some(task) = self.get(session_id, task_id).await? else {
            return Err(format!("no task {task_id} in this conversation"));
        };
        if task.state.is_terminal() {
            return Ok(());
        }
        let token = self
            .inner
            .runtimes()
            .get(&task_id)
            .map(RuntimeHandle::cancellation_token);
        match token {
            Some(token) => {
                token.cancel();
                Ok(())
            }
            None => match self.get(session_id, task_id).await? {
                Some(latest) if latest.state.is_terminal() => Ok(()),
                _ => Err(format!(
                    "task {task_id} is not running in this process; cross-process control is not \
                     available yet"
                )),
            },
        }
    }
}

const PROCESS_HEAD_BYTES: usize = 8 * 1024;
const PROCESS_TAIL_BYTES: usize = 24 * 1024;

const NOTIFY_HEAD_BYTES: usize = 2 * 1024;
const NOTIFY_TAIL_BYTES: usize = 6 * 1024;

fn due_at(
    schedule: &Schedule,
    now: chrono::DateTime<Utc>,
) -> Result<Option<chrono::DateTime<Utc>>, String> {
    match schedule {
        Schedule::Manual => Ok(None),
        Schedule::Once { at } => Ok((*at <= now).then_some(*at)),
        Schedule::Cron {
            expression,
            timezone,
        } => {
            let timezone = timezone
                .parse::<Tz>()
                .map_err(|_| format!("unknown timezone {timezone}"))?;
            let spec = CronSpec::parse(expression)?;
            let local = now.with_timezone(&timezone);
            if !spec.matches(&local) {
                return Ok(None);
            }
            Ok(now
                .with_second(0)
                .and_then(|value| value.with_nanosecond(0)))
        }
    }
}

/// Five-field cron matcher (`minute hour day-of-month month day-of-week`).
/// Lists, ranges and steps are supported. Keeping this parser here makes the accepted syntax
/// identical for validation and execution, and avoids a scheduler whose UI accepts expressions
/// the runner later interprets differently.
#[derive(Debug)]
struct CronSpec {
    minute: CronField,
    hour: CronField,
    day: CronField,
    month: CronField,
    weekday: CronField,
}

impl CronSpec {
    fn parse(expression: &str) -> Result<Self, String> {
        let fields = expression.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 5 {
            return Err(
                "cron must have 5 fields: minute hour day-of-month month day-of-week".into(),
            );
        }
        Ok(Self {
            minute: CronField::parse(fields[0], 0, 59, "minute")?,
            hour: CronField::parse(fields[1], 0, 23, "hour")?,
            day: CronField::parse(fields[2], 1, 31, "day-of-month")?,
            month: CronField::parse(fields[3], 1, 12, "month")?,
            weekday: CronField::parse(fields[4], 0, 6, "day-of-week")?,
        })
    }

    fn matches<TzLike: chrono::TimeZone>(&self, value: &chrono::DateTime<TzLike>) -> bool {
        let day_matches = self.day.contains(value.day());
        let weekday_matches = self
            .weekday
            .contains(value.weekday().num_days_from_sunday());
        // Traditional cron treats day-of-month and day-of-week as OR when both are restricted.
        let calendar_matches = if !self.day.wildcard && !self.weekday.wildcard {
            day_matches || weekday_matches
        } else {
            day_matches && weekday_matches
        };
        self.minute.contains(value.minute())
            && self.hour.contains(value.hour())
            && self.month.contains(value.month())
            && calendar_matches
    }
}

#[derive(Debug)]
struct CronField {
    min: u32,
    allowed: Vec<bool>,
    wildcard: bool,
}

impl CronField {
    fn parse(input: &str, min: u32, max: u32, label: &str) -> Result<Self, String> {
        let mut allowed = vec![false; (max - min + 1) as usize];
        for part in input.split(',') {
            if part.is_empty() {
                return Err(format!("{label} contains an empty value"));
            }
            let (base, step) = match part.split_once('/') {
                Some((base, step)) => {
                    let step = step
                        .parse::<u32>()
                        .map_err(|_| format!("invalid step in {label}: {part}"))?;
                    if step == 0 {
                        return Err(format!("step in {label} cannot be 0"));
                    }
                    (base, step)
                }
                None => (part, 1),
            };
            let (start, end) = if base == "*" {
                (min, max)
            } else if let Some((start, end)) = base.split_once('-') {
                (
                    parse_cron_number(start, min, max, label)?,
                    parse_cron_number(end, min, max, label)?,
                )
            } else {
                let value = parse_cron_number(base, min, max, label)?;
                (value, value)
            };
            if start > end {
                return Err(format!(
                    "range start cannot be greater than end in {label}: {part}"
                ));
            }
            for value in (start..=end).step_by(step as usize) {
                allowed[(value - min) as usize] = true;
            }
        }
        Ok(Self {
            min,
            allowed,
            wildcard: input == "*",
        })
    }

    fn contains(&self, value: u32) -> bool {
        value
            .checked_sub(self.min)
            .and_then(|index| self.allowed.get(index as usize))
            .copied()
            .unwrap_or(false)
    }
}

fn parse_cron_number(input: &str, min: u32, max: u32, label: &str) -> Result<u32, String> {
    let value = input
        .parse::<u32>()
        .map_err(|_| format!("invalid value in {label}: {input}"))?;
    if !(min..=max).contains(&value) {
        return Err(format!("{label} must be between {min}..={max}"));
    }
    Ok(value)
}

fn read_spool_preview(path: &Path) -> Result<String, String> {
    read_spool_window(path, PROCESS_HEAD_BYTES, PROCESS_TAIL_BYTES)
}

fn read_spool_window(path: &Path, head_bytes: usize, tail_bytes: usize) -> Result<String, String> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error.to_string()),
    };
    let bytes = file.metadata().map_err(|error| error.to_string())?.len() as usize;
    if bytes <= head_bytes + tail_bytes {
        let mut all = Vec::with_capacity(bytes);
        file.read_to_end(&mut all)
            .map_err(|error| error.to_string())?;
        return Ok(String::from_utf8_lossy(&all).into_owned());
    }

    let mut head = vec![0; head_bytes];
    file.read_exact(&mut head)
        .map_err(|error| error.to_string())?;
    file.seek(SeekFrom::End(-(tail_bytes as i64)))
        .map_err(|error| error.to_string())?;
    let mut tail = vec![0; tail_bytes];
    file.read_exact(&mut tail)
        .map_err(|error| error.to_string())?;
    Ok(format!(
        "{}\n\n[{} bytes omitted from the middle; head and tail kept]\n\n{}",
        String::from_utf8_lossy(&head),
        bytes - head_bytes - tail_bytes,
        String::from_utf8_lossy(&tail)
    ))
}

async fn terminate_process(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new("kill")
            .arg("-TERM")
            .arg("--")
            .arg(format!("-{pid}"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        if tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .is_ok()
        {
            return;
        }
    }
    let _ = child.start_kill();
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use zlogic_protocol::{CallId, TurnId, WorkspaceId};
    use zlogic_store::{Db, NewSession};
    use zlogic_tools::{AgentOutcome, CancellationToken};

    use super::*;

    fn manager(
        store: SharedStore,
    ) -> (
        tempfile::TempDir,
        Arc<zlogic_objects::MemoryObjectStore>,
        TaskManager,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let objects = Arc::new(zlogic_objects::MemoryObjectStore::new());
        let manager = TaskManager::new(store, objects.clone(), dir.path()).unwrap();
        (dir, objects, manager)
    }

    struct FixedSpawner {
        called: AtomicBool,
    }

    struct MailboxSpawner {
        store: SharedStore,
        received: Mutex<Option<oneshot::Sender<String>>>,
    }

    #[derive(Default)]
    struct RecordingWake {
        sessions: Mutex<Vec<SessionId>>,
    }

    impl TaskWake for RecordingWake {
        fn mailbox_ready(&self, session_id: SessionId) -> Result<(), String> {
            self.sessions.lock().unwrap().push(session_id);
            Ok(())
        }
    }

    #[test]
    fn startup_reconciliation_interrupts_active_runs_and_notifies_the_task_session() {
        let store = SharedStore::new(Db::open_in_memory().unwrap());
        let session = store
            .with(|db| db.sessions().create(NewSession::task(WorkspaceId::new())))
            .unwrap();
        let (_spool, _objects, manager) = manager(store.clone());
        let task = store
            .with(|db| {
                let tasks = TaskStore::new(db.conn());
                let task = tasks.create(NewTask {
                    job_id: None,
                    workspace_id: session.workspace_id,
                    executor: ExecutorSpec::Agent(AgentSpec {
                        prompt: "continue later".into(),
                        agent: "reviewer".into(),
                        model_ref: None,
                        cwd: None,
                    }),
                    permission_policy: PermissionPolicy::DenyRequests,
                    notification_session_id: Some(session.session_id),
                    trigger: TaskTrigger::Manual,
                    turn_scoped: false,
                    attempt: 1,
                    scheduled_for: None,
                })?;
                tasks.transition(
                    task.task_id,
                    TaskState::Queued,
                    TaskState::Running,
                    None,
                    None,
                )
            })
            .unwrap();

        assert_eq!(manager.reconcile_interrupted().unwrap(), 1);
        let recovered = store
            .with(|db| TaskStore::new(db.conn()).get(task.task_id))
            .unwrap();
        assert_eq!(recovered.state, TaskState::Interrupted);
        assert_eq!(
            recovered.error.as_deref(),
            Some("runtime restarted before the task completed")
        );
        let pending = store
            .with(|db| db.mailbox().pending(session.session_id))
            .unwrap();
        assert_eq!(pending.len(), 1);
        let parts: Vec<MessagePart> = serde_json::from_value(pending[0].parts.clone()).unwrap();
        assert!(matches!(
            parts.as_slice(),
            [MessagePart::TaskUpdate { update }]
                if update.task_id == task.task_id.to_string()
                    && update.state == "interrupted"
        ));
    }

    #[async_trait]
    impl AgentSpawner for FixedSpawner {
        async fn spawn(&self, req: AgentRequest) -> Result<AgentOutcome, String> {
            self.called.store(true, Ordering::SeqCst);
            assert!(req.unattended);
            Ok(AgentOutcome {
                session_id: zlogic_protocol::SessionId::new(),
                answer: "background conclusion".into(),
            })
        }

        fn available(&self) -> Vec<String> {
            vec!["reviewer".into()]
        }
    }

    #[async_trait]
    impl AgentSpawner for MailboxSpawner {
        async fn spawn(&self, req: AgentRequest) -> Result<AgentOutcome, String> {
            let child = self
                .store
                .with(|db| {
                    db.sessions()
                        .create(NewSession::child(req.parent_session_id, &req.agent))
                })
                .map_err(|error| error.to_string())?;
            req.mailbox
                .as_ref()
                .expect("background agent has a mailbox gate")
                .activate(child.session_id)
                .await;

            let message = loop {
                let pending = self
                    .store
                    .with(|db| db.mailbox().pending(child.session_id))
                    .map_err(|error| error.to_string())?;
                if let Some(record) = pending.first() {
                    let parts: Vec<MessagePart> = serde_json::from_value(record.parts.clone())
                        .map_err(|error| error.to_string())?;
                    if let Some(MessagePart::Text { text }) = parts.first() {
                        break text.clone();
                    }
                }
                tokio::task::yield_now().await;
            };
            if let Some(sender) = self.received.lock().unwrap().take() {
                let _ = sender.send(message);
            }
            Ok(AgentOutcome {
                session_id: child.session_id,
                answer: "updated".into(),
            })
        }

        fn available(&self) -> Vec<String> {
            vec!["reviewer".into()]
        }
    }

    struct FixedAgentFactory {
        spawner: Arc<FixedSpawner>,
    }

    #[async_trait]
    impl ScheduledAgentFactory for FixedAgentFactory {
        async fn spawner_for(
            &self,
            _workspace_id: WorkspaceId,
            _parent_session_id: SessionId,
            profile: &str,
            _model_ref: Option<&str>,
            _cwd: Option<&str>,
        ) -> Result<Arc<dyn AgentSpawner>, String> {
            if profile != "reviewer" {
                return Err(format!("unknown profile {profile}"));
            }
            Ok(self.spawner.clone())
        }
    }

    #[tokio::test]
    async fn background_agent_is_persisted_and_finishes() {
        let store = SharedStore::new(Db::open_in_memory().unwrap());
        let session = store
            .with(|db| db.sessions().create(NewSession::root(WorkspaceId::new())))
            .unwrap();
        let (_spool, _objects, manager) = manager(store.clone());
        let wake = Arc::new(RecordingWake::default());
        let wake_trait: Arc<dyn TaskWake> = wake.clone();
        manager.bind_waker(Arc::downgrade(&wake_trait));
        let spawner = Arc::new(FixedSpawner {
            called: AtomicBool::new(false),
        });

        let task_id = manager
            .start_agent(
                AgentRequest {
                    agent: "reviewer".into(),
                    task: "review it".into(),
                    parent_session_id: session.session_id,
                    parent_turn_id: TurnId::new(),
                    exec_cwd: None,
                    mailbox: None,
                    anchor_call_id: CallId::new("call_bg"),
                    unattended: true,
                    cancel: CancellationToken::new(),
                    system: None,
                    tools: None,
                    model: None,
                },
                spawner.clone(),
            )
            .await
            .unwrap();

        let finished = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let task = manager
                    .get(session.session_id, task_id)
                    .await
                    .unwrap()
                    .unwrap();
                if task.state.is_terminal() && manager.runtime_count() == 0 {
                    break task;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(finished.state, TaskState::Succeeded);
        assert!(matches!(
            finished.result,
            Some(TaskResult::Agent(AgentResult {
                conclusion: Some(ref answer),
                ..
            })) if answer == "background conclusion"
        ));
        assert!(spawner.called.load(Ordering::SeqCst));
        assert_eq!(manager.runtime_count(), 0);
        assert_eq!(*wake.sessions.lock().unwrap(), [session.session_id]);
        let pending = manager
            .store()
            .with(|db| db.mailbox().pending(session.session_id))
            .unwrap();
        assert_eq!(pending.len(), 1);
        let parts: Vec<MessagePart> = serde_json::from_value(pending[0].parts.clone()).unwrap();
        assert!(matches!(
            parts.as_slice(),
            [MessagePart::TaskUpdate { update }]
                if update.task_id == task_id.to_string()
                    && update.state == "succeeded"
                    && update.summary.as_deref() == Some("background conclusion")
                    && update.child_session_id.is_some()
        ));
    }

    #[tokio::test]
    async fn message_sent_while_background_agent_starts_reaches_its_child_mailbox() {
        let store = SharedStore::new(Db::open_in_memory().unwrap());
        let session = store
            .with(|db| db.sessions().create(NewSession::root(WorkspaceId::new())))
            .unwrap();
        let (_spool, _objects, manager) = manager(store.clone());
        let (received_tx, received_rx) = oneshot::channel();
        let spawner = Arc::new(MailboxSpawner {
            store,
            received: Mutex::new(Some(received_tx)),
        });

        let task_id = manager
            .start_agent(
                AgentRequest {
                    agent: "reviewer".into(),
                    task: "review it".into(),
                    parent_session_id: session.session_id,
                    parent_turn_id: TurnId::new(),
                    exec_cwd: None,
                    mailbox: None,
                    anchor_call_id: CallId::new("call_message"),
                    unattended: true,
                    cancel: CancellationToken::new(),
                    system: None,
                    tools: None,
                    model: None,
                },
                spawner,
            )
            .await
            .unwrap();

        // Deliberately send immediately: the gate must wait for child-session activation rather
        // than intermittently rejecting or losing this message.
        manager
            .send_agent_message(session.session_id, task_id, "also check the tests".into())
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), received_rx)
                .await
                .unwrap()
                .unwrap(),
            "also check the tests"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_process_spools_output_and_persists_object() {
        use std::process::Stdio;

        let store = SharedStore::new(Db::open_in_memory().unwrap());
        let session = store
            .with(|db| db.sessions().create(NewSession::root(WorkspaceId::new())))
            .unwrap();
        let (_spool, objects, manager) = manager(store.clone());
        let hub = Arc::new(crate::hub::EventHub::new());
        manager.bind_hub(Arc::downgrade(&hub));
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "printf hello; printf error >&2"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let task_id = manager
            .start_process(
                ProcessRequest {
                    spec: zlogic_task::ProcessSpec {
                        program: "/bin/sh".into(),
                        args: vec!["-c".into(), "printf hello; printf error >&2".into()],
                        cwd: None,
                        env: std::collections::BTreeMap::new(),
                    },
                    parent_session_id: session.session_id,
                    parent_turn_id: TurnId::new(),
                    anchor_call_id: CallId::new("call_process"),
                    cancel: zlogic_tools::CancellationToken::new(),
                    turn_scoped: true,
                },
                SpawnedProcess {
                    child,
                    stdout,
                    stderr,
                },
            )
            .await
            .unwrap();

        let finished = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let task = manager
                    .get(session.session_id, task_id)
                    .await
                    .unwrap()
                    .unwrap();
                if task.state.is_terminal() && manager.runtime_count() == 0 {
                    break task;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(finished.state, TaskState::Succeeded);
        let TaskResult::Process(result) = finished.result.unwrap() else {
            panic!("expected process result");
        };
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.output_chars, 10);
        let object_id = result.output_object_id.unwrap();
        let stored = objects.get(&object_id).unwrap();
        let stored = String::from_utf8(stored).unwrap();
        assert!(stored.contains("hello"));
        assert!(stored.contains("error"));

        let (console, _) = hub.subscribe_task(&task_id.to_string());
        let console: String = console.iter().map(|delta| delta.chunk.as_str()).collect();
        assert!(console.contains("hello"), "{console}");
        assert!(console.contains("error"), "{console}");

        let pending = store
            .with(|db| db.mailbox().pending(session.session_id))
            .unwrap();
        assert_eq!(pending.len(), 1, "one completion notification");
        let decoded = zlogic_core::steer::decode_parts(&pending[0].parts);
        let preview = decoded.iter().find_map(|part| match part {
            zlogic_core::steer::DecodedPart::TaskUpdate(update) => update.preview.clone(),
            _ => None,
        });
        let preview = preview.expect("notification carries an output preview");
        assert!(preview.contains("hello"), "{preview}");
        assert!(preview.contains("error"), "{preview}");
    }

    struct WaitingSpawner;

    #[async_trait]
    impl AgentSpawner for WaitingSpawner {
        async fn spawn(&self, req: AgentRequest) -> Result<AgentOutcome, String> {
            req.cancel.cancelled().await;
            Err("cancelled".into())
        }

        fn available(&self) -> Vec<String> {
            vec!["reviewer".into()]
        }
    }

    #[tokio::test]
    async fn stop_cancels_the_runtime_and_persists_cancelled() {
        let store = SharedStore::new(Db::open_in_memory().unwrap());
        let session = store
            .with(|db| db.sessions().create(NewSession::root(WorkspaceId::new())))
            .unwrap();
        let (_spool, _objects, manager) = manager(store.clone());
        let task_id = manager
            .start_agent(
                AgentRequest {
                    agent: "reviewer".into(),
                    task: "wait".into(),
                    parent_session_id: session.session_id,
                    parent_turn_id: TurnId::new(),
                    exec_cwd: None,
                    mailbox: None,
                    anchor_call_id: CallId::new("call_stop"),
                    unattended: true,
                    cancel: CancellationToken::new(),
                    system: None,
                    tools: None,
                    model: None,
                },
                Arc::new(WaitingSpawner),
            )
            .await
            .unwrap();

        manager.stop(session.session_id, task_id).await.unwrap();
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let task = manager
                    .get(session.session_id, task_id)
                    .await
                    .unwrap()
                    .unwrap();
                if task.state.is_terminal() && manager.runtime_count() == 0 {
                    break task;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(cancelled.state, TaskState::Cancelled);
        assert_eq!(manager.runtime_count(), 0);
        assert_eq!(
            store
                .with(|db| db.mailbox().pending(session.session_id))
                .unwrap()
                .len(),
            1,
            "an explicit stop still notifies the conversation"
        );
    }

    #[tokio::test]
    async fn a_stop_the_turn_caused_cancels_the_task_without_waking_the_session() {
        let store = SharedStore::new(Db::open_in_memory().unwrap());
        let session = store
            .with(|db| db.sessions().create(NewSession::root(WorkspaceId::new())))
            .unwrap();
        let (_spool, _objects, manager) = manager(store.clone());
        let wake = Arc::new(RecordingWake::default());
        let wake_trait: Arc<dyn TaskWake> = wake.clone();
        manager.bind_waker(Arc::downgrade(&wake_trait));

        let turn = CancellationToken::new();
        let task_id = manager
            .start_agent(
                AgentRequest {
                    agent: "reviewer".into(),
                    task: "wait".into(),
                    parent_session_id: session.session_id,
                    parent_turn_id: TurnId::new(),
                    exec_cwd: None,
                    mailbox: None,
                    anchor_call_id: CallId::new("call_turn_stop"),
                    unattended: true,
                    cancel: turn.clone(),
                    system: None,
                    tools: None,
                    model: None,
                },
                Arc::new(WaitingSpawner),
            )
            .await
            .unwrap();

        turn.cancel();
        let finished = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let task = manager
                    .get(session.session_id, task_id)
                    .await
                    .unwrap()
                    .unwrap();
                if task.state.is_terminal() && manager.runtime_count() == 0 {
                    break task;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(
            finished.state,
            TaskState::Cancelled,
            "the task still stops, and still records its state"
        );
        assert!(
            store
                .with(|db| db.mailbox().pending(session.session_id))
                .unwrap()
                .is_empty(),
            "a stop the turn caused itself must not queue a notification"
        );
        assert!(
            wake.sessions.lock().unwrap().is_empty(),
            "and must not wake the session into a new turn"
        );
    }

    /// A sub-agent that creates its child session, then fails mid-run.
    struct FailingSpawner {
        child_session_id: SessionId,
    }

    #[async_trait]
    impl AgentSpawner for FailingSpawner {
        async fn spawn(&self, req: AgentRequest) -> Result<AgentOutcome, String> {
            req.mailbox
                .as_ref()
                .expect("background agent has a mailbox gate")
                .activate(self.child_session_id)
                .await;
            Err("upstream blew up".into())
        }

        fn available(&self) -> Vec<String> {
            vec!["reviewer".into()]
        }
    }

    #[tokio::test]
    async fn failed_agent_keeps_the_link_to_its_persisted_transcript() {
        let store = SharedStore::new(Db::open_in_memory().unwrap());
        let session = store
            .with(|db| db.sessions().create(NewSession::root(WorkspaceId::new())))
            .unwrap();
        let (_spool, _objects, manager) = manager(store.clone());
        let child = store
            .with(|db| {
                db.sessions()
                    .create(NewSession::child(session.session_id, "reviewer"))
            })
            .unwrap();
        let task_id = manager
            .start_agent(
                AgentRequest {
                    agent: "reviewer".into(),
                    task: "review it".into(),
                    parent_session_id: session.session_id,
                    parent_turn_id: TurnId::new(),
                    exec_cwd: None,
                    mailbox: None,
                    anchor_call_id: CallId::new("call_fail"),
                    unattended: true,
                    cancel: CancellationToken::new(),
                    system: None,
                    tools: None,
                    model: None,
                },
                Arc::new(FailingSpawner {
                    child_session_id: child.session_id,
                }),
            )
            .await
            .unwrap();

        let failed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let task = manager
                    .get(session.session_id, task_id)
                    .await
                    .unwrap()
                    .unwrap();
                if task.state.is_terminal() && manager.runtime_count() == 0 {
                    break task;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(failed.state, TaskState::Failed);
        assert_eq!(
            failed.error.as_deref(),
            Some("upstream blew up"),
            "failure reason must survive on the run"
        );
        assert_eq!(
            failed.result,
            Some(TaskResult::Agent(AgentResult {
                child_session_id: child.session_id,
                final_entry_id: None,
                conclusion: None,
            })),
            "failed agent must still carry its child session so the transcript is reachable"
        );

        // task_log must expose the child session id; the UI pulls the transcript with it.
        let log = TaskService::task_log(
            &manager,
            RuntimeTaskLogReq {
                workspace_id: session.workspace_id,
                task_id: task_id.to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            log.child_session_id.as_deref(),
            Some(child.session_id.to_string().as_str())
        );
        assert_eq!(log.state, RuntimeTaskState::Failed);
        assert_eq!(log.error.as_deref(), Some("upstream blew up"));
    }

    #[tokio::test]
    async fn runtime_task_query_keeps_active_complete_and_pages_terminal_history() {
        let store = SharedStore::new(Db::open_in_memory().unwrap());
        let session = store
            .with(|db| db.sessions().create(NewSession::root(WorkspaceId::new())))
            .unwrap();
        let (_spool, _objects, manager) = manager(store.clone());
        let create = |label: &str| NewTask {
            job_id: None,
            workspace_id: session.workspace_id,
            executor: ExecutorSpec::Agent(AgentSpec {
                prompt: label.into(),
                agent: "reviewer".into(),
                model_ref: None,
                cwd: None,
            }),
            permission_policy: PermissionPolicy::DenyRequests,
            notification_session_id: Some(session.session_id),
            trigger: TaskTrigger::Manual,
            turn_scoped: false,
            attempt: 1,
            scheduled_for: None,
        };
        store
            .with(|db| TaskStore::new(db.conn()).create(create("active")))
            .unwrap();
        for label in ["done one", "done two"] {
            store
                .with(|db| {
                    let tasks = TaskStore::new(db.conn());
                    let task = tasks.create(create(label))?;
                    tasks.transition(
                        task.task_id,
                        TaskState::Queued,
                        TaskState::Running,
                        None,
                        None,
                    )?;
                    tasks.transition(
                        task.task_id,
                        TaskState::Running,
                        TaskState::Succeeded,
                        Some(TaskResult::Agent(AgentResult {
                            child_session_id: SessionId::new(),
                            final_entry_id: None,
                            conclusion: Some(label.into()),
                        })),
                        None,
                    )
                })
                .unwrap();
        }

        let page = TaskService::list_tasks(
            &manager,
            RuntimeTaskListReq {
                workspace_id: session.workspace_id,
                stopped_offset: 0,
                stopped_limit: Some(1),
                only_job_tasks: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(page.active.len(), 1);
        assert_eq!(page.stopped.len(), 1);
        assert_eq!(page.stopped_total, 2);
        assert_eq!(page.active[0].title, "active");

        let job_only = TaskService::list_tasks(
            &manager,
            RuntimeTaskListReq {
                workspace_id: session.workspace_id,
                stopped_offset: 0,
                stopped_limit: Some(20),
                only_job_tasks: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(job_only.active.len(), 0);
        assert_eq!(job_only.stopped.len(), 0);
        assert_eq!(job_only.stopped_total, 0);
    }

    #[test]
    fn cron_supports_lists_ranges_and_steps_in_the_requested_timezone() {
        use chrono::TimeZone;

        let spec = CronSpec::parse("*/15 9-17 * * 1-5").unwrap();
        let monday = chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2026, 8, 3, 9, 30, 0)
            .unwrap();
        assert!(spec.matches(&monday));
        assert!(!spec.matches(&monday.with_minute(31).unwrap()));
        assert!(
            !spec.matches(
                &chrono_tz::Asia::Shanghai
                    .with_ymd_and_hms(2026, 8, 2, 9, 30, 0)
                    .unwrap()
            )
        );
    }

    #[test]
    fn due_at_uses_local_cron_time_and_returns_a_stable_utc_minute() {
        use chrono::TimeZone;

        let now = Utc.with_ymd_and_hms(2026, 8, 3, 1, 0, 42).unwrap();
        let schedule = Schedule::Cron {
            expression: "0 9 * * 1-5".into(),
            timezone: "Asia/Shanghai".into(),
        };
        assert_eq!(
            due_at(&schedule, now).unwrap(),
            Some(Utc.with_ymd_and_hms(2026, 8, 3, 1, 0, 0).unwrap())
        );
    }

    #[tokio::test]
    async fn job_api_persists_manual_and_scheduled_definitions() {
        let store = SharedStore::new(Db::open_in_memory().unwrap());
        let root = tempfile::tempdir().unwrap();
        let workspace = store
            .with(|db| db.workspaces().resolve(root.path()).map(|value| value.0))
            .unwrap();
        let (_spool, _objects, manager) = manager(store.clone());

        let created = TaskService::create_job(
            &manager,
            TaskJobCreateReq {
                workspace_id: workspace.workspace_id,
                task_session_id: None,
                title: "weekday build".into(),
                executor: TaskJobExecutor::Process {
                    program: "cargo".into(),
                    args: vec!["check".into()],
                    cwd: None,
                },
                schedule: TaskJobSchedule::Cron {
                    expression: "0 9 * * 1-5".into(),
                    timezone: "Asia/Shanghai".into(),
                },
                enabled: true,
                concurrency_policy: TaskJobConcurrencyPolicy::Forbid,
            },
        )
        .await
        .unwrap();

        let task_session = store
            .with(|db| db.sessions().get(created.task_session_id))
            .unwrap();
        assert!(task_session.is_task_session());
        assert!(
            store
                .with(|db| db.sessions().list(workspace.workspace_id))
                .unwrap()
                .is_empty(),
            "task sessions must not appear in the normal chat list"
        );

        let jobs = TaskService::list_jobs(
            &manager,
            TaskJobListReq {
                workspace_id: workspace.workspace_id,
            },
        )
        .await
        .unwrap();
        assert_eq!(jobs, vec![created]);
    }

    #[tokio::test]
    async fn agent_job_uses_the_bound_production_factory() {
        let store = SharedStore::new(Db::open_in_memory().unwrap());
        let root = tempfile::tempdir().unwrap();
        let workspace = store
            .with(|db| db.workspaces().resolve(root.path()).map(|value| value.0))
            .unwrap();
        let (_spool, _objects, manager) = manager(store);
        let spawner = Arc::new(FixedSpawner {
            called: AtomicBool::new(false),
        });
        let factory = Arc::new(FixedAgentFactory {
            spawner: spawner.clone(),
        });
        let factory_trait: Arc<dyn ScheduledAgentFactory> = factory;
        manager.bind_agent_factory(Arc::downgrade(&factory_trait));

        let job = TaskService::create_job(
            &manager,
            TaskJobCreateReq {
                workspace_id: workspace.workspace_id,
                task_session_id: None,
                title: "review".into(),
                executor: TaskJobExecutor::Agent {
                    profile: "reviewer".into(),
                    prompt: "review the workspace".into(),
                    cwd: None,
                    model_ref: None,
                },
                schedule: TaskJobSchedule::Manual,
                enabled: true,
                concurrency_policy: TaskJobConcurrencyPolicy::Forbid,
            },
        )
        .await
        .unwrap();
        let run = TaskService::run_job(
            &manager,
            TaskJobRunReq {
                job_id: job.job_id.clone(),
            },
        )
        .await
        .unwrap();

        let finished = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let task = <TaskManager as TaskHost>::get(
                    &manager,
                    job.task_session_id,
                    run.task_id.parse().unwrap(),
                )
                .await
                .unwrap()
                .unwrap();
                if task.state.is_terminal() {
                    break task;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(finished.state, TaskState::Succeeded);
        assert!(spawner.called.load(Ordering::SeqCst));
    }
}

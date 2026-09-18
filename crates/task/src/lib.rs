//! Persistent jobs and task runs.
//! This crate deliberately has no dependency on `zlogic-core`, `zlogic-engine`, or `zlogic-store`.
//! A host can install [`SCHEMA`] into its SQLite database and construct [`JobStore`] /
//! [`TaskStore`] from a borrowed connection. Starting work is also outside this crate:
//! [`RuntimeHandle`] only owns one live future's cancellation and join handles.
//! The three lifetimes are intentionally separate:
//! - [`JobDefinition`] is durable user intent and may create many runs.
//! - [`TaskRun`] is one durable execution attempt and owns an executor snapshot.
//! - [`RuntimeHandle`] is process-local and disappears on restart.

mod ids;
mod model;
mod runtime;
mod store;

pub use ids::{JobId, TaskId};
pub use model::{
    AgentResult, AgentSpec, ConcurrencyPolicy, ExecutorSpec, JobDefinition, JobOwner, NewJob,
    NewTask, PermissionPolicy, ProcessResult, ProcessSpec, Schedule, TaskResult, TaskRun,
    TaskState, TaskTrigger,
};
pub use runtime::RuntimeHandle;
pub use store::{JobStore, SCHEMA, StoreError, TaskStore, install_schema};

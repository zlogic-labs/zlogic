//! Which listening port belongs to which background run.
//!
//! A background task's row says what was started, not what it opened: `bash -c "npm run dev"` binds
//! 5173 three generations down and the run itself never touches a socket. The only place that is
//! written down is the operating system's listen table, and the only thing that connects it back to
//! a run is the pid the run was spawned with.
//!
//! # Why the pid is not in the database
//!
//! A pid says a process existed. It cannot say it still does — the number is recycled — and a
//! persisted pid would be a claim the database cannot keep honest after a crash. So the pid lives
//! in process memory beside the runtime handle that owns it, is dropped the moment the run ends,
//! and a run belonging to another engine process has no entry at all. That last case is not a
//! degradation to paper over: this engine cannot speak for a process it did not start, and
//! reporting no ports is the honest answer rather than a guess.
//!
//! # Why nothing is cached
//!
//! The ports themselves are recomputed on every read. A remembered port outlives its process, and a
//! badge pointing at whatever now holds that number is worse than no badge. It also means a run
//! that binds late — a dev server that takes thirty seconds to compile — simply appears when it
//! does, with no timer to get wrong.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use zlogic_task::TaskId;

/// The root pid of every background process task this engine process is running.
#[derive(Debug, Default)]
pub struct ProcessRoots {
    roots: Mutex<HashMap<TaskId, u32>>,
}

impl ProcessRoots {
    pub fn new() -> Self {
        Self::default()
    }

    /// Notes the pid a run was spawned with. Called once, at adoption.
    pub fn record(&self, task_id: TaskId, pid: u32) {
        self.lock().insert(task_id, pid);
    }

    /// Drops a run's pid. A run that never had one is not an error: agent tasks have no process.
    pub fn forget(&self, task_id: &TaskId) {
        self.lock().remove(task_id);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// The ports each of `wanted` is listening on, right now.
    ///
    /// Only runs with a known pid appear in the result. The whole process tree is read once and
    /// the listen table once, so the cost is two snapshots rather than a walk per candidate.
    pub fn listening_ports(&self, wanted: &[TaskId]) -> HashMap<TaskId, Vec<u16>> {
        if wanted.is_empty() {
            return HashMap::new();
        }
        let roots = self.lock();
        let mut by_pid: HashMap<u32, TaskId> = HashMap::new();
        let mut root_pids: Vec<u32> = Vec::with_capacity(wanted.len());
        for &task_id in wanted {
            if let Some(&pid) = roots.get(&task_id) {
                if by_pid.insert(pid, task_id).is_none() {
                    root_pids.push(pid);
                }
            }
        }
        drop(roots);
        if root_pids.is_empty() {
            return HashMap::new();
        }

        let owners = zlogic_nettable::descendant_roots(&zlogic_nettable::process_parents(), &root_pids);
        let mut out: HashMap<TaskId, Vec<u16>> = HashMap::new();
        for socket in zlogic_nettable::listening_sockets() {
            let Some(&root) = owners.get(&socket.pid) else {
                continue;
            };
            let Some(&task_id) = by_pid.get(&root) else {
                continue;
            };
            out.entry(task_id).or_default().push(socket.port);
        }
        for ports in out.values_mut() {
            ports.sort_unstable();
            ports.dedup();
        }
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TaskId, u32>> {
        self.roots.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_with_no_pid_reports_no_ports() {
        let roots = ProcessRoots::new();
        let task_id = TaskId::new();
        assert!(roots.listening_ports(&[task_id]).is_empty());
    }

    #[test]
    fn asking_about_nothing_does_not_read_the_system() {
        // The guard is behavioural, not an assertion about output: a poll for an idle conversation
        // must not pay for a process snapshot.
        assert!(ProcessRoots::new().listening_ports(&[]).is_empty());
    }

    /// A pid that the caller already knows is still gone the moment the run is.
    #[test]
    fn forgetting_a_run_takes_its_pid_with_it() {
        let roots = ProcessRoots::new();
        let task_id = TaskId::new();
        roots.record(task_id, std::process::id());
        assert_eq!(roots.len(), 1);
        roots.forget(&task_id);
        assert_eq!(roots.len(), 0);
        roots.forget(&task_id);
    }
}

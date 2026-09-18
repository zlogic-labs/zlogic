use std::sync::Arc;
use std::time::{Duration, SystemTime};

use zlogic_objects::{ObjectId, ObjectStore};
use zlogic_store::SharedStore;

#[derive(Debug, Clone)]
pub struct GcOptions {
    pub grace: Duration,
    pub now: SystemTime,
    pub dry_run: bool,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(24 * 60 * 60),
            now: SystemTime::now(),
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    pub scanned: u64,
    pub referenced: u64,
    pub too_young: u64,
    pub deleted: u64,
    pub bytes_freed: u64,
    pub delete_failed: u64,
}

impl GcReport {
    pub fn summary(&self) -> String {
        format!(
            "scanned {} objects: {} referenced, {} too young, deleted {} (freed {} bytes), \
             {} failed to delete",
            self.scanned,
            self.referenced,
            self.too_young,
            self.deleted,
            self.bytes_freed,
            self.delete_failed,
        )
    }
}

pub fn sweep(
    store: &SharedStore,
    objects: &Arc<dyn ObjectStore>,
    opts: &GcOptions,
) -> Result<GcReport, String> {
    let mut report = GcReport::default();

    let mut candidates: Vec<(ObjectId, u64)> = Vec::new();
    objects
        .scan(&mut |entry| {
            report.scanned += 1;
            let age = opts
                .now
                .duration_since(entry.stored_at)
                .unwrap_or(Duration::ZERO);
            if age < opts.grace {
                report.too_young += 1;
            } else {
                candidates.push((entry.id, entry.size));
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;

    if candidates.is_empty() {
        return Ok(report);
    }

    let mut live: std::collections::HashSet<ObjectId> = store
        .with(|db| db.entries().all_referenced_objects())
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    live.extend(
        store
            .with(|db| zlogic_task::TaskStore::new(db.conn()).output_objects())
            .map_err(|e| e.to_string())?,
    );

    for (id, size) in candidates {
        if live.contains(&id) {
            report.referenced += 1;
            continue;
        }
        let still_unreferenced = store
            .with(|db| {
                Ok::<_, String>(
                    !db.entries()
                        .is_object_referenced(&id)
                        .map_err(|error| error.to_string())?
                        && !zlogic_task::TaskStore::new(db.conn())
                            .references_output(&id)
                            .map_err(|error| error.to_string())?,
                )
            })
            .unwrap_or(false);
        if !still_unreferenced {
            report.referenced += 1;
            continue;
        }
        if opts.dry_run {
            report.deleted += 1;
            report.bytes_freed += size;
            continue;
        }
        match objects.delete(&id) {
            Ok(true) => {
                report.deleted += 1;
                report.bytes_freed += size;
            }
            Ok(false) => report.delete_failed += 1,
            Err(e) => {
                tracing::debug!(target: "zlogic::gc", "failed to delete {id}: {e}");
                report.delete_failed += 1;
            }
        }
    }

    Ok(report)
}

pub fn spawn_detached(
    store: SharedStore,
    objects: Arc<dyn ObjectStore>,
    min_interval: chrono::Duration,
    delay: Duration,
) -> Option<std::thread::JoinHandle<Option<GcReport>>> {
    std::thread::Builder::new()
        .name("zlogic-object-gc".into())
        .spawn(move || {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }

            match store.with(|db| {
                db.maintenance().claim(
                    zlogic_store::maintenance::OBJECT_GC,
                    min_interval,
                    chrono::Utc::now(),
                )
            }) {
                Ok(false) => {
                    tracing::debug!(target: "zlogic::gc", "too soon since last sweep, skipping");
                    return None;
                }
                Err(e) => {
                    tracing::warn!(target: "zlogic::gc", "failed to claim sweep: {e}");
                    return None;
                }
                Ok(true) => {}
            }

            match sweep(&store, &objects, &GcOptions::default()) {
                Ok(report) => {
                    tracing::info!(target: "zlogic::gc", "{}", report.summary());
                    Some(report)
                }
                Err(e) => {
                    tracing::warn!(target: "zlogic::gc", "sweep failed: {e}");
                    None
                }
            }
        })
        .inspect_err(|e| tracing::warn!(target: "zlogic::gc", "failed to start sweep thread: {e}"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_objects::{MemoryObjectStore, ObjectRef, ObjectRole};
    use zlogic_store::{Db, EntryKind, NewEntry, NewSession, SharedStore};

    fn setup() -> (
        SharedStore,
        Arc<dyn ObjectStore>,
        zlogic_protocol::SessionId,
    ) {
        let store = SharedStore::new(Db::open_in_memory().unwrap());
        store
            .with(|db| zlogic_task::install_schema(db.conn()))
            .unwrap();
        let session = store.with(|db| {
            let (ws, _) = db.workspaces().resolve("/work").unwrap();
            db.sessions()
                .create(NewSession::root(ws.workspace_id))
                .unwrap()
                .session_id
        });
        (store, Arc::new(MemoryObjectStore::new()), session)
    }

    fn put_referenced(
        store: &SharedStore,
        objects: &Arc<dyn ObjectStore>,
        session: zlogic_protocol::SessionId,
        bytes: &[u8],
    ) -> ObjectId {
        let id = objects.put(bytes).unwrap();
        store
            .with(|db| {
                db.entries().append(
                    NewEntry::new(
                        session,
                        zlogic_protocol::TurnId::new(),
                        1,
                        EntryKind::ToolResult,
                        serde_json::json!({ "text": "x" }),
                    )
                    .references(ObjectRef::new(id.clone(), ObjectRole::Output)),
                )
            })
            .unwrap();
        id
    }

    fn young() -> GcOptions {
        GcOptions {
            grace: Duration::from_secs(3600),
            now: SystemTime::now(),
            dry_run: false,
        }
    }

    fn aged() -> GcOptions {
        GcOptions {
            grace: Duration::from_secs(3600),
            now: SystemTime::now() + Duration::from_secs(48 * 3600),
            dry_run: false,
        }
    }

    #[test]
    fn an_unreferenced_object_is_deleted() {
        let (store, objects, _) = setup();
        let id = objects.put(b"orphan").unwrap();

        let report = sweep(&store, &objects, &aged()).unwrap();
        assert_eq!(report.deleted, 1);
        assert_eq!(report.bytes_freed, 6);
        assert!(!objects.exists(&id).unwrap());
    }

    #[test]
    fn a_referenced_object_survives() {
        let (store, objects, session) = setup();
        let id = put_referenced(&store, &objects, session, b"kept");

        let report = sweep(&store, &objects, &aged()).unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(report.referenced, 1);
        assert!(objects.exists(&id).unwrap());
    }

    #[test]
    fn a_process_task_output_object_survives() {
        let (store, objects, _) = setup();
        let id = objects.put(b"durable process output").unwrap();
        store
            .with(|db| {
                let tasks = zlogic_task::TaskStore::new(db.conn());
                let task = tasks.create(zlogic_task::NewTask::manual(
                    zlogic_protocol::WorkspaceId::new(),
                    zlogic_task::ExecutorSpec::Process(zlogic_task::ProcessSpec {
                        program: "test".into(),
                        args: Vec::new(),
                        cwd: None,
                        env: std::collections::BTreeMap::new(),
                    }),
                ))?;
                tasks.transition(
                    task.task_id,
                    zlogic_task::TaskState::Queued,
                    zlogic_task::TaskState::Running,
                    None,
                    None,
                )?;
                tasks.transition(
                    task.task_id,
                    zlogic_task::TaskState::Running,
                    zlogic_task::TaskState::Succeeded,
                    Some(zlogic_task::TaskResult::Process(
                        zlogic_task::ProcessResult {
                            exit_code: Some(0),
                            output_object_id: Some(id.clone()),
                            output_chars: 22,
                        },
                    )),
                    None,
                )?;
                Ok::<_, zlogic_task::StoreError>(())
            })
            .unwrap();

        let report = sweep(&store, &objects, &aged()).unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(report.referenced, 1);
        assert!(objects.exists(&id).unwrap());
    }

    #[test]
    fn a_freshly_stored_object_is_never_touched() {
        let (store, objects, _) = setup();
        let id = objects
            .put(b"just written, reference not yet recorded")
            .unwrap();

        let report = sweep(&store, &objects, &young()).unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(
            report.too_young, 1,
            "the reason it was kept has to be 'too young', not luck"
        );
        assert!(objects.exists(&id).unwrap());
    }

    #[test]
    fn the_report_accounts_for_every_object() {
        let (store, objects, session) = setup();
        put_referenced(&store, &objects, session, b"referenced");
        objects.put(b"orphan one").unwrap();
        objects.put(b"orphan two").unwrap();

        let report = sweep(&store, &objects, &aged()).unwrap();
        assert_eq!(report.scanned, 3);
        assert_eq!(
            report.referenced + report.too_young + report.deleted + report.delete_failed,
            3
        );
        assert_eq!(report.deleted, 2);
    }

    #[test]
    fn a_dry_run_deletes_nothing() {
        let (store, objects, _) = setup();
        let id = objects.put(b"orphan").unwrap();

        let opts = GcOptions {
            dry_run: true,
            ..aged()
        };
        let report = sweep(&store, &objects, &opts).unwrap();
        assert_eq!(report.deleted, 1, "the report says it would be deleted");
        assert!(objects.exists(&id).unwrap(), "but it is still there");
    }

    #[test]
    fn an_object_becomes_collectable_once_its_entry_is_gone() {
        let (store, objects, session) = setup();
        let id = put_referenced(&store, &objects, session, b"was referenced");

        assert_eq!(sweep(&store, &objects, &aged()).unwrap().deleted, 0);

        store.with(|db| db.sessions().delete(session)).unwrap();
        let report = sweep(&store, &objects, &aged()).unwrap();
        assert_eq!(
            report.deleted, 1,
            "the reference is gone, so the object is garbage"
        );
        assert!(!objects.exists(&id).unwrap());
    }

    #[test]
    fn an_empty_store_sweeps_to_nothing() {
        let (store, objects, _) = setup();
        assert_eq!(
            sweep(&store, &objects, &aged()).unwrap(),
            GcReport::default()
        );
    }

    #[test]
    fn a_store_that_cannot_enumerate_is_an_error_not_an_empty_sweep() {
        struct NoScan;
        impl ObjectStore for NoScan {
            fn algo(&self) -> zlogic_objects::HashAlgo {
                zlogic_objects::HashAlgo::Sha256
            }
            fn put(&self, _b: &[u8]) -> zlogic_objects::Result<ObjectId> {
                unimplemented!()
            }
            fn put_path(&self, _p: &std::path::Path) -> zlogic_objects::Result<ObjectId> {
                unimplemented!()
            }
            fn open(
                &self,
                _id: &ObjectId,
            ) -> zlogic_objects::Result<Box<dyn std::io::Read + Send>> {
                unimplemented!()
            }
            fn exists(&self, _id: &ObjectId) -> zlogic_objects::Result<bool> {
                unimplemented!()
            }
            fn size(&self, _id: &ObjectId) -> zlogic_objects::Result<u64> {
                unimplemented!()
            }
        }
        let (store, _, _) = setup();
        let objects: Arc<dyn ObjectStore> = Arc::new(NoScan);
        assert!(sweep(&store, &objects, &aged()).is_err());
    }

    #[test]
    fn the_startup_sweep_runs_once_and_then_debounces() {
        let (store, objects, _) = setup();
        objects.put(b"orphan").unwrap();

        let first = spawn_detached(
            store.clone(),
            objects.clone(),
            chrono::Duration::hours(24),
            Duration::ZERO,
        )
        .expect("the thread should start")
        .join()
        .unwrap();
        assert!(first.is_some(), "the first one should really run");

        let second = spawn_detached(
            store.clone(),
            objects.clone(),
            chrono::Duration::hours(24),
            Duration::ZERO,
        )
        .expect("the thread should start")
        .join()
        .unwrap();
        assert!(
            second.is_none(),
            "the second one is held back by the dedupe"
        );
    }

    #[test]
    fn a_failing_sweep_is_swallowed() {
        let (store, _, _) = setup();
        struct Broken;
        impl ObjectStore for Broken {
            fn algo(&self) -> zlogic_objects::HashAlgo {
                zlogic_objects::HashAlgo::Sha256
            }
            fn put(&self, _b: &[u8]) -> zlogic_objects::Result<ObjectId> {
                unimplemented!()
            }
            fn put_path(&self, _p: &std::path::Path) -> zlogic_objects::Result<ObjectId> {
                unimplemented!()
            }
            fn open(
                &self,
                _id: &ObjectId,
            ) -> zlogic_objects::Result<Box<dyn std::io::Read + Send>> {
                unimplemented!()
            }
            fn exists(&self, _id: &ObjectId) -> zlogic_objects::Result<bool> {
                unimplemented!()
            }
            fn size(&self, _id: &ObjectId) -> zlogic_objects::Result<u64> {
                unimplemented!()
            }
            fn scan(
                &self,
                _v: &mut dyn FnMut(zlogic_objects::ObjectEntry) -> zlogic_objects::Result<()>,
            ) -> zlogic_objects::Result<()> {
                Err(zlogic_objects::ObjectError::Backend(
                    "the store is broken".into(),
                ))
            }
        }

        let out = spawn_detached(
            store,
            Arc::new(Broken),
            chrono::Duration::zero(),
            Duration::ZERO,
        )
        .expect("the thread should start")
        .join();
        assert!(out.is_ok(), "the task itself must not panic");
        assert!(out.unwrap().is_none());
    }
}

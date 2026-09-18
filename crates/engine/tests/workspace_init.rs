//! Opening a workspace has to report `Started` and then `Done`.
//!
//! The UI keeps its "opening workspace" progress up until it sees a terminal phase, so an open
//! that reports nothing leaves it waiting forever, and one that ends in `Cancelled` never
//! finishes at all. The first full snapshot that used to produce these reports is gone: opening a
//! workspace is now a plain registration, and the open path reports its progress itself.

use std::path::Path;
use std::sync::Arc;

use zlogic_engine::Workspaces;
use zlogic_engine::hub::EventHub;
use zlogic_engine::service::WorkspaceService;
use zlogic_protocol::query::WorkspaceSelector;
use zlogic_protocol::stream::{InitPhase, WorkspaceInitProgress};
use zlogic_store::{Db, SharedStore};

fn store() -> SharedStore {
    SharedStore::new(Db::open_in_memory().unwrap())
}

fn path_sel(dir: &Path) -> WorkspaceSelector {
    WorkspaceSelector::Path {
        root: dir.to_string_lossy().into_owned(),
    }
}

/// The phases of one open, up to and including the phase that ends it.
async fn phases_of_open(
    rx: &mut tokio::sync::broadcast::Receiver<WorkspaceInitProgress>,
) -> Vec<InitPhase> {
    let mut seen = Vec::new();
    loop {
        let p = tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv())
            .await
            .expect("the open never reported anything; the UI would wait forever")
            .expect("the channel is closed");
        let terminal = !matches!(p.phase, InitPhase::Started | InitPhase::Scanning { .. });
        seen.push(p.phase);
        if terminal {
            return seen;
        }
    }
}

fn assert_reported_an_open(seen: &[InitPhase]) {
    assert_eq!(
        seen.first(),
        Some(&InitPhase::Started),
        "an open has to announce itself before it ends: {seen:?}"
    );
    assert!(
        matches!(seen.last(), Some(InitPhase::Done { .. })),
        "the UI only stops waiting on `Done`, never on `Cancelled`: {seen:?}"
    );
}

#[tokio::test]
async fn opening_a_folder_registers_it_and_reports_the_pair() {
    let work = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(work.path().join("src")).unwrap();
    std::fs::write(work.path().join("src/main.rs"), "fn main() {}").unwrap();

    let hub = Arc::new(EventHub::new());
    let workspaces = Workspaces::new(store()).with_hub(hub.clone());
    let mut rx = hub.subscribe_inits();

    let summary = workspaces
        .resolve(path_sel(work.path()), None, None)
        .await
        .unwrap();

    assert_reported_an_open(&phases_of_open(&mut rx).await);

    let again = workspaces
        .resolve(path_sel(work.path()), None, None)
        .await
        .unwrap();
    assert_eq!(
        again.workspace_id, summary.workspace_id,
        "opening it a second time has to resolve to the same workspace"
    );
}

#[tokio::test]
async fn reopening_a_registered_folder_reports_the_pair_again() {
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("a.rs"), "x").unwrap();

    let hub = Arc::new(EventHub::new());
    let workspaces = Workspaces::new(store()).with_hub(hub.clone());

    workspaces
        .resolve(path_sel(work.path()), None, None)
        .await
        .unwrap();

    let mut rx = hub.subscribe_inits();
    workspaces
        .resolve(path_sel(work.path()), None, None)
        .await
        .unwrap();

    assert_reported_an_open(&phases_of_open(&mut rx).await);
}

#[tokio::test]
async fn the_cli_open_path_reports_the_pair_too() {
    let work = tempfile::tempdir().unwrap();

    let hub = Arc::new(EventHub::new());
    let workspaces = Workspaces::new(store()).with_hub(hub.clone());
    let mut rx = hub.subscribe_inits();

    workspaces.open_at(work.path()).unwrap();

    assert_reported_an_open(&phases_of_open(&mut rx).await);
}

#[tokio::test]
async fn a_headless_open_still_registers_the_workspace() {
    let work = tempfile::tempdir().unwrap();
    let workspaces = Workspaces::new(store());

    let summary = workspaces
        .resolve(path_sel(work.path()), None, None)
        .await
        .unwrap();
    let again = workspaces
        .resolve(path_sel(work.path()), None, None)
        .await
        .unwrap();

    assert_eq!(again.workspace_id, summary.workspace_id);
}

//! A workspace is often opened as the parent of several repositories, and the files a session
//! edits are almost always inside one of them. A walk that stops at a nested `.git` reports every
//! such file as absent, so the timeline fills up with snapshots that differ from the workspace by
//! nothing at all.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use zlogic_checkpoints::{Capture, Checkpoints, Config, Snapshot, Trigger};

struct Fixture {
    _base: tempfile::TempDir,
    work: PathBuf,
    store: Arc<Checkpoints>,
}

impl Fixture {
    fn new() -> Self {
        let base = tempfile::tempdir().unwrap();
        let work = base.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git2::Repository::init(&work).unwrap();
        write(&work, "beside.txt", "x\n");
        Self {
            store: Checkpoints::new(base.path().join("store"), Config::default()),
            work,
            _base: base,
        }
    }

    async fn capture(&self) -> Snapshot {
        self.store
            .capture(
                self.work.clone(),
                Capture {
                    session: "s".into(),
                    turn: Some("t".into()),
                    trigger: Trigger::TurnStart,
                    tool: None,
                    detail: None,
                    label: None,
                },
            )
            .await
            .unwrap()
    }

    async fn plan(&self, id: &str) -> zlogic_checkpoints::RestorePlan {
        self.store
            .plan_restore(self.work.clone(), id.into(), "s".into())
            .await
            .unwrap()
    }
}

fn write(root: &Path, path: &str, body: &str) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, body).unwrap();
}

#[tokio::test]
async fn a_nested_repository_is_walked_rather_than_skipped() {
    let fixture = Fixture::new();
    let nested = fixture.work.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    git2::Repository::init(&nested).unwrap();
    write(&nested, "inner.txt", "x\n");

    let point = fixture.capture().await;
    write(&nested, "inner.txt", "x\nchanged\n");
    let plan = fixture.plan(&point.id).await;

    let mut writes: Vec<&str> = plan
        .writes
        .iter()
        .map(|write| write.path.as_str())
        .collect();
    writes.sort();
    assert_eq!(writes, vec!["nested/inner.txt"], "{writes:?}");
}

#[tokio::test]
async fn a_nested_repository_keeps_its_own_ignore_rules() {
    let fixture = Fixture::new();
    let nested = fixture.work.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    git2::Repository::init(&nested).unwrap();
    write(&nested, ".gitignore", "skip/\n");
    write(&nested, "skip/ignored.txt", "x\n");
    write(&nested, "kept.txt", "x\n");

    let point = fixture.capture().await;
    write(&nested, "skip/ignored.txt", "x\nchanged\n");
    let plan = fixture.plan(&point.id).await;

    // `beside.txt`, the nested `.gitignore` and the nested `kept.txt`: the three files that are
    // content. `skip/ignored.txt` is none of them, so the file the session changed is not a
    // restore point.
    assert_eq!(plan.unchanged, 3, "{:?}", plan);
    assert!(plan.writes.is_empty(), "{:?}", plan.writes);
    assert!(
        std::fs::read_to_string(nested.join("skip/ignored.txt"))
            .unwrap()
            .contains("changed")
    );
    let _ = point;
}

#[tokio::test]
async fn a_nested_repository_is_restored_from_its_own_path() {
    let fixture = Fixture::new();
    let nested = fixture.work.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    git2::Repository::init(&nested).unwrap();
    write(&nested, "deep/inner.txt", "x\noriginal\n");

    let point = fixture.capture().await;
    write(&nested, "deep/inner.txt", "x\nchanged\n");
    let plan = fixture.plan(&point.id).await;
    fixture
        .store
        .apply_restore(
            fixture.work.clone(),
            plan,
            zlogic_checkpoints::RestoreOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(nested.join("deep/inner.txt")).unwrap(),
        "x\noriginal\n"
    );
}

#[tokio::test]
async fn a_parent_rule_stops_at_a_nested_repositorys_own_boundary() {
    // A repository is a self-contained ignore scope: the ancestor chain of `.gitignore` files is
    // walked from the inside out and cut at the first directory with a `.git` of its own, so a
    // rule written above a repository never hides a file that repository tracks. The `ignore`
    // crate's `saw_git` is what cuts it. A rule the other way round — the nested repository's own
    // `.gitignore` — does apply, which is the case that decides what is stored.
    let fixture = Fixture::new();
    write(&fixture.work, ".gitignore", "nested/skip/\n");
    let nested = fixture.work.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    git2::Repository::init(&nested).unwrap();
    write(&nested, "skip/hidden.txt", "x\n");
    write(&nested, "kept.txt", "x\n");

    let point = fixture.capture().await;
    write(&nested, "skip/hidden.txt", "x\nchanged\n");
    write(&nested, "kept.txt", "x\nchanged\n");
    let plan = fixture.plan(&point.id).await;

    let mut writes: Vec<&str> = plan
        .writes
        .iter()
        .map(|write| write.path.as_str())
        .collect();
    writes.sort();
    assert_eq!(
        writes,
        vec!["nested/kept.txt", "nested/skip/hidden.txt"],
        "{writes:?}"
    );
}

#[tokio::test]
async fn a_gitignore_outside_any_repository_still_applies() {
    let fixture = Fixture::new();
    let plain = fixture.work.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    write(&plain, ".gitignore", "skip/\n");
    write(&plain, "skip/hidden.txt", "x\n");
    write(&plain, "kept.txt", "x\n");

    let point = fixture.capture().await;
    write(&plain, "skip/hidden.txt", "x\nchanged\n");
    write(&plain, "kept.txt", "x\nchanged\n");
    let plan = fixture.plan(&point.id).await;

    let mut writes: Vec<&str> = plan
        .writes
        .iter()
        .map(|write| write.path.as_str())
        .collect();
    writes.sort();
    assert_eq!(writes, vec!["plain/kept.txt"], "{writes:?}");
}

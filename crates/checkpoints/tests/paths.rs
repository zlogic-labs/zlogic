//! Path handling, because it is where this crate's two worst bugs lived.
//!
//! Both of these were Windows-only and both were invisible to a smoke test that only used files
//! in the repository root: a snapshot keyed its stale-file pass on the OS path bytes, so every
//! file below the root was added and then immediately deleted again; and the tree walk that
//! computes a plan kept only each entry's leaf name, so a restore of `dir/sub/two.txt` named a
//! path that does not exist. Both would have shipped as "the checkpoint works" and a restore that
//! quietly writes to the wrong place.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use zlogic_checkpoints::{Capture, Checkpoints, Config, RestoreOptions, Snapshot, Trigger};

struct Fixture {
    _base: tempfile::TempDir,
    work: PathBuf,
    store: Arc<Checkpoints>,
}

impl Fixture {
    /// A repository with one commit, so `HEAD` exists and a restore has a same-head check to pass.
    fn new(tracked: &[&str]) -> Self {
        let base = tempfile::tempdir().unwrap();
        let work = base.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let user = git2::Repository::init(&work).unwrap();
        for path in tracked {
            // A `.gitignore` written as body text would ignore nothing, and the test that
            // depends on one would pass for the wrong reason.
            let body = if path.ends_with(".gitignore") {
                "skip/\n"
            } else {
                "x\n"
            };
            write(&work, path, body);
        }
        let mut index = user.index().unwrap();
        for path in tracked {
            index.add_path(Path::new(path)).unwrap();
        }
        index.write().unwrap();
        let tree = user.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("t", "t@example.invalid").unwrap();
        user.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
            .unwrap();
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

    async fn step(&self, id: &str) -> zlogic_checkpoints::StepDiff {
        self.store.step(self.work.clone(), id.into()).await.unwrap()
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.work.join(path)).unwrap()
    }
}

fn write(root: &Path, path: &str, body: &str) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, body).unwrap();
}

#[tokio::test]
async fn a_snapshot_keeps_the_files_below_the_root() {
    let fixture = Fixture::new(&["a.txt", "dir/one.txt", "dir/sub/two.txt"]);
    let point = fixture.capture().await;

    // Change one line in each, so each is a write rather than a no-op.
    write(&fixture.work, "dir/one.txt", "x\nchanged\n");
    write(&fixture.work, "dir/sub/two.txt", "x\nchanged\n");
    let plan = fixture.plan(&point.id).await;

    let mut writes: Vec<&str> = plan
        .writes
        .iter()
        .map(|write| write.path.as_str())
        .collect();
    writes.sort();
    assert_eq!(writes, vec!["dir/one.txt", "dir/sub/two.txt"], "{writes:?}");
}

#[tokio::test]
async fn a_page_walks_the_whole_chain_without_repeating_a_row() {
    let fixture = Fixture::new(&["a.txt"]);
    for n in 0..5 {
        write(&fixture.work, "a.txt", &format!("x\n{n}\n"));
        fixture.capture().await;
    }
    let all = fixture.store.list(fixture.work.clone()).await.unwrap();
    assert_eq!(all.len(), 5, "each write is a distinct tree");

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = fixture
            .store
            .list_page(
                fixture.work.clone(),
                cursor.clone(),
                2,
                Some("master".into()),
            )
            .await
            .unwrap();
        assert_eq!(page.total, 5, "the total is the chain, not the page");
        seen.extend(page.items.iter().map(|snapshot| snapshot.id.clone()));
        if !page.has_more {
            break;
        }
        cursor = page.items.last().map(|snapshot| snapshot.id.clone());
    }
    let expected: Vec<String> = all.iter().map(|snapshot| snapshot.id.clone()).collect();
    assert_eq!(seen, expected, "every row exactly once, newest first");
}

#[tokio::test]
async fn a_page_cursor_the_sweep_dropped_still_returns_rows() {
    let fixture = Fixture::new(&["a.txt"]);
    for n in 0..3 {
        write(&fixture.work, "a.txt", &format!("x\n{n}\n"));
        fixture.capture().await;
    }
    let page = fixture
        .store
        .list_page(
            fixture.work.clone(),
            Some("0".repeat(40)),
            10,
            Some("master".into()),
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 3, "an unknown cursor pages from the top");
    assert!(!page.has_more);
}

#[tokio::test]
async fn a_page_for_one_branch_leaves_the_other_branches_out_and_counts_them() {
    let fixture = Fixture::new(&["a.txt"]);
    for n in 0..3 {
        write(&fixture.work, "a.txt", &format!("x\n{n}\n"));
        fixture.capture().await;
    }
    // Three snapshots on this branch, and a page for a branch none of them were taken on: the
    // filter and the count are both about the same chain seen two ways.
    let all = fixture.store.list(fixture.work.clone()).await.unwrap();
    assert_eq!(all.len(), 3);

    let page = fixture
        .store
        .list_page(fixture.work.clone(), None, 10, Some("feature/other".into()))
        .await
        .unwrap();
    assert!(
        page.items.is_empty(),
        "nothing was taken on that branch, so the page is empty"
    );
    assert_eq!(page.total, 0, "the count is the branch's, not the chain's");
    assert_eq!(
        page.other_branches, 3,
        "but the other-branch count is the chain's"
    );
    assert!(!page.has_more);
}

#[tokio::test]
async fn a_restore_writes_to_the_path_the_snapshot_names() {
    let fixture = Fixture::new(&["dir/sub/two.txt"]);
    let point = fixture.capture().await;
    write(&fixture.work, "dir/sub/two.txt", "x\nchanged\n");

    fixture
        .store
        .apply_restore(
            fixture.work.clone(),
            fixture.plan(&point.id).await,
            RestoreOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(fixture.read("dir/sub/two.txt"), "x\n");
    // The failure this guards against was a write to `<root>/two.txt`.
    assert!(!fixture.work.join("two.txt").exists());
}

#[tokio::test]
async fn a_file_diff_is_the_patch_behind_the_row_that_offers_it() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let point = fixture.capture().await;
    write(&fixture.work, "a.txt", "x\nadded\n");
    write(&fixture.work, "b.txt", "x\nchanged\n");

    let patch = fixture
        .store
        .file_diff(
            fixture.work.clone(),
            point.id.clone(),
            "a.txt".into(),
            zlogic_checkpoints::Compare::Workspace,
        )
        .await
        .unwrap();
    assert!(!patch.binary);
    assert!(
        patch.unified.contains("+added"),
        "the patch says what the workspace has that the point did not: {}",
        patch.unified
    );
    assert!(
        !patch.unified.contains("b.txt"),
        "one path asked for, one path answered: {}",
        patch.unified
    );

    let untouched = fixture
        .store
        .file_diff(
            fixture.work.clone(),
            point.id.clone(),
            "b.txt".into(),
            zlogic_checkpoints::Compare::Workspace,
        )
        .await
        .unwrap();
    assert!(
        untouched.unified.contains("+changed") && !untouched.unified.contains("+added"),
        "{}",
        untouched.unified
    );

    // A file the workspace no longer has still has its snapshot side to compare against.
    std::fs::remove_file(fixture.work.join("a.txt")).unwrap();
    let gone = fixture
        .store
        .file_diff(
            fixture.work.clone(),
            point.id.clone(),
            "a.txt".into(),
            zlogic_checkpoints::Compare::Workspace,
        )
        .await
        .unwrap();
    assert!(
        gone.unified.contains("-x") || gone.unified.contains("-added"),
        "{}",
        gone.unified
    );
}

#[tokio::test]
async fn a_plan_names_every_file_even_when_no_dialog_could_show_them() {
    // The wire caps its rows so a list stays renderable, and the cap lives there on purpose: a
    // restore writes every file the plan names, so a cap inside the store would turn "restore the
    // workspace" into "restore 200 files and stop".
    let tracked: Vec<String> = (0..250).map(|n| format!("f{n}.txt")).collect();
    let tracked: Vec<&str> = tracked.iter().map(String::as_str).collect();
    let fixture = Fixture::new(&tracked);
    let point = fixture.capture().await;
    for n in 0..250 {
        write(&fixture.work, &format!("f{n}.txt"), "changed\n");
    }

    let plan = fixture.plan(&point.id).await;
    assert_eq!(plan.writes.len(), 250, "the plan is whole");
    assert_eq!(plan.writes_total, 250);
    let step = fixture.step(&point.id).await;
    assert_eq!(
        step.files_total, 0,
        "nothing changed before the first point"
    );
}

#[tokio::test]
async fn a_row_reads_as_the_step_that_ended_at_it() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let first = fixture.capture().await;
    write(&fixture.work, "a.txt", "x\nstep one\n");
    let second = fixture.capture().await;
    write(&fixture.work, "b.txt", "x\nstep two\n");
    let third = fixture.capture().await;

    // The first point has nothing before it, which the view says rather than showing as an empty
    // step the user would read as "nothing happened here".
    let step = fixture.step(&first.id).await;
    assert!(step.previous.is_none());
    assert!(step.files.is_empty());

    let step = fixture.step(&second.id).await;
    assert_eq!(step.previous.as_deref(), Some(first.id.as_str()));
    let mut touched: Vec<&str> = step.files.iter().map(|f| f.path.as_str()).collect();
    touched.sort();
    assert_eq!(touched, vec!["a.txt"], "{touched:?}");
    assert_eq!(step.drift.insertions, 1);
    assert_eq!(
        step.files[0].lines.unwrap().insertions,
        1,
        "the row's own count, not the tree's"
    );

    let step = fixture.step(&third.id).await;
    let mut touched: Vec<&str> = step.files.iter().map(|f| f.path.as_str()).collect();
    touched.sort();
    assert_eq!(touched, vec!["b.txt"], "{touched:?}");
    assert!(step.deletions.is_empty());
}

#[tokio::test]
async fn a_binary_file_is_stored_and_reported_as_unchanged_by_line_count() {
    let fixture = Fixture::new(&["a.txt", "logo.png"]);
    let first = fixture.capture().await;
    write(&fixture.work, "logo.png", "\u{0}\u{1}PNG\u{0}\u{2}");
    let second = fixture.capture().await;
    write(&fixture.work, "a.txt", "x\n");

    // The bytes are kept — a restore has to be able to put an image back — but there is nothing to
    // count lines in, so the row carries no numbers rather than a made-up zero.
    let step = fixture.step(&second.id).await;
    assert_eq!(step.files.len(), 1, "{:?}", step.files);
    assert_eq!(step.files[0].path, "logo.png");
    assert!(
        step.files[0].lines.is_none(),
        "a binary has no lines to count"
    );
    assert_eq!(step.drift, zlogic_checkpoints::LineStats::default());

    let patch = fixture
        .store
        .file_diff(
            fixture.work.clone(),
            second.id.clone(),
            "logo.png".into(),
            zlogic_checkpoints::Compare::Previous,
        )
        .await
        .unwrap();
    assert!(patch.binary, "the UI says so instead of rendering mojibake");
    assert!(
        !patch.unified.contains('\u{1}'),
        "no line body for a binary: {:?}",
        patch.unified
    );

    fixture
        .store
        .apply_restore(
            fixture.work.clone(),
            fixture.plan(&second.id).await,
            RestoreOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(fixture.read("logo.png"), "\u{0}\u{1}PNG\u{0}\u{2}");
    let _ = first;
}

#[tokio::test]
async fn a_step_patch_and_a_workspace_patch_say_different_things() {
    let fixture = Fixture::new(&["a.txt"]);
    let first = fixture.capture().await;
    write(&fixture.work, "a.txt", "x\nby the step\n");
    let second = fixture.capture().await;
    write(&fixture.work, "a.txt", "x\nby the step\nand after it\n");

    let step = fixture
        .store
        .file_diff(
            fixture.work.clone(),
            second.id.clone(),
            "a.txt".into(),
            zlogic_checkpoints::Compare::Previous,
        )
        .await
        .unwrap();
    assert!(
        step.unified.contains("+by the step") && !step.unified.contains("and after it"),
        "the step patch stops where the step stopped: {}",
        step.unified
    );

    let workspace = fixture
        .store
        .file_diff(
            fixture.work.clone(),
            second.id.clone(),
            "a.txt".into(),
            zlogic_checkpoints::Compare::Workspace,
        )
        .await
        .unwrap();
    assert!(
        workspace.unified.contains("+and after it"),
        "the workspace patch is measured against now: {}",
        workspace.unified
    );
    let _ = first;
}

#[tokio::test]
async fn files_sharing_a_leaf_name_are_different_files() {
    let fixture = Fixture::new(&["dir/one.txt", "elsewhere/one.txt"]);
    let point = fixture.capture().await;
    write(&fixture.work, "dir/one.txt", "a\n");
    write(&fixture.work, "elsewhere/one.txt", "b\n");
    let plan = fixture.plan(&point.id).await;

    let mut writes: Vec<&str> = plan
        .writes
        .iter()
        .map(|write| write.path.as_str())
        .collect();
    writes.sort();
    assert_eq!(
        writes,
        vec!["dir/one.txt", "elsewhere/one.txt"],
        "{writes:?}"
    );
    assert!(
        plan.writes
            .iter()
            .all(|write| write.lines.unwrap().insertions == 1),
        "each file's own line count, not a merged one"
    );
}

#[tokio::test]
async fn ignored_files_are_neither_stored_nor_deleted() {
    let fixture = Fixture::new(&["a.txt", ".gitignore"]);
    write(&fixture.work, "skip/hidden.txt", "no\n");
    let point = fixture.capture().await;
    write(&fixture.work, "a.txt", "x\nchanged\n");
    write(&fixture.work, "skip/hidden.txt", "changed but ignored\n");

    let plan = fixture.plan(&point.id).await;
    assert!(
        plan.writes
            .iter()
            .all(|write| !write.path.starts_with("skip/")),
        "an ignored file is not in the snapshot, so there is nothing to write: {:?}",
        plan.writes
    );
    assert!(plan.deletes.iter().all(|path| !path.starts_with("skip/")));

    fixture
        .store
        .apply_restore(
            fixture.work.clone(),
            plan,
            RestoreOptions {
                delete_new: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(fixture.read("skip/hidden.txt"), "changed but ignored\n");
}

#[tokio::test]
async fn a_file_the_tree_gained_survives_until_the_caller_asks_for_it() {
    let fixture = Fixture::new(&["a.txt"]);
    let point = fixture.capture().await;
    write(&fixture.work, "dir/new.txt", "added\n");

    let plan = fixture.plan(&point.id).await;
    assert_eq!(plan.deletes, vec!["dir/new.txt".to_string()]);

    fixture
        .store
        .apply_restore(fixture.work.clone(), plan, RestoreOptions::default())
        .await
        .unwrap();
    assert!(fixture.work.join("dir/new.txt").exists(), "kept by default");

    let plan = fixture.plan(&point.id).await;
    let outcome = fixture
        .store
        .apply_restore(
            fixture.work.clone(),
            plan,
            RestoreOptions {
                delete_new: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.deleted, 1);
    assert!(!fixture.work.join("dir/new.txt").exists());
}

#[tokio::test]
async fn a_single_file_restore_touches_nothing_else() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let point = fixture.capture().await;
    write(&fixture.work, "a.txt", "x\nwrecked\n");
    write(&fixture.work, "b.txt", "y\nalso wrecked\n");
    write(&fixture.work, "added.txt", "new\n");

    let outcome = fixture
        .store
        .apply_restore(
            fixture.work.clone(),
            fixture.plan(&point.id).await,
            RestoreOptions {
                only: Some("a.txt".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!((outcome.written, outcome.deleted), (1, 0));
    assert!(outcome.guard.is_some(), "a restore is undoable");
    assert_eq!(fixture.read("a.txt"), "x\n");
    assert_eq!(fixture.read("b.txt"), "y\nalso wrecked\n", "untouched");
    assert!(fixture.work.join("added.txt").exists(), "untouched");
}

/// A restore writes file by file, and one file it cannot write does not stop the others. What has
/// to survive that is *which* file and *why* — a count with no names is not something the user
/// can act on, and the paths are git paths, which may contain the separator a client would
/// otherwise split a pre-formatted message on.
#[tokio::test]
async fn a_file_a_restore_cannot_write_is_named_alongside_the_ones_it_can() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let point = fixture.capture().await;
    write(&fixture.work, "a.txt", "wrecked\n");
    write(&fixture.work, "b.txt", "also wrecked\n");

    // A non-empty directory where the snapshot names a file: removable by nothing, so the write
    // has to fail. Portable — removing a directory as a file fails on every platform we run on.
    std::fs::remove_file(fixture.work.join("a.txt")).unwrap();
    std::fs::create_dir_all(fixture.work.join("a.txt/inside")).unwrap();
    write(&fixture.work, "a.txt/inside/keep.txt", "keep\n");

    let outcome = fixture
        .store
        .apply_restore(
            fixture.work.clone(),
            fixture.plan(&point.id).await,
            RestoreOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.written, 1, "the other file still gets written");
    assert_eq!(
        fixture.read("b.txt"),
        "x\n",
        "the write that could happen did"
    );
    assert_eq!(
        outcome.failed.len(),
        1,
        "and the one that could not is reported, not swallowed"
    );
    assert_eq!(outcome.failed[0].path, "a.txt");
    assert!(
        !outcome.failed[0].error.is_empty(),
        "a failure without a reason is not an answer"
    );
    assert!(
        fixture.work.join("a.txt/inside/keep.txt").exists(),
        "a failed write leaves the path it collided with alone"
    );
}

#[tokio::test]
async fn a_label_is_one_line_because_a_trailer_is_one_line() {
    let fixture = Fixture::new(&["a.txt"]);
    let point = fixture
        .store
        .capture(
            fixture.work.clone(),
            Capture {
                session: "s".into(),
                turn: None,
                trigger: Trigger::Manual,
                tool: None,
                detail: Some("git checkout -- .\r\nand a second line".into()),
                label: Some("before\nthe refactor".into()),
            },
        )
        .await
        .unwrap();

    // The record returned here and the one a list reads must be the same text, or a label would
    // change the moment the timeline was reopened.
    let listed = fixture.store.list(fixture.work.clone()).await.unwrap();
    let same = listed.iter().find(|entry| entry.id == point.id).unwrap();
    assert_eq!(same.label, point.label);
    assert_eq!(same.detail, point.detail);
    assert!(!point.label.as_deref().unwrap().contains('\n'));
    assert_eq!(point.label.as_deref(), Some("before the refactor"));
    assert_eq!(
        point.detail.as_deref(),
        Some("git checkout -- . and a second line")
    );
}

#[tokio::test]
async fn a_moved_head_is_refused_unless_the_caller_says_otherwise() {
    let fixture = Fixture::new(&["a.txt"]);
    let point = fixture.capture().await;
    let user = git2::Repository::open(&fixture.work).unwrap();

    let head = user.head().unwrap().target().unwrap();
    let tree = user.head().unwrap().peel_to_tree().unwrap();
    let signature = git2::Signature::now("t", "t@example.invalid").unwrap();
    let parent = user.find_commit(head).unwrap();
    let second = user
        .commit(None, &signature, &signature, "moved on", &tree, &[&parent])
        .unwrap();
    user.reference("refs/heads/master", second, true, "moved")
        .unwrap();

    let plan = fixture.plan(&point.id).await;
    assert!(
        !plan.head_matches,
        "the plan says so before anything is written"
    );
    let refused = fixture
        .store
        .apply_restore(
            fixture.work.clone(),
            plan.clone(),
            RestoreOptions::default(),
        )
        .await;
    assert!(
        refused.is_err(),
        "a restore across a moved HEAD needs consent"
    );

    let outcome = fixture
        .store
        .apply_restore(
            fixture.work.clone(),
            plan,
            RestoreOptions {
                cross_head: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
}
